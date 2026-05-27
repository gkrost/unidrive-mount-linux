use crate::ipc::{IpcError, ListEntry};
use crate::reconnect::ReconnectingIpcClient;
use crate::path_map::{PathMap, ROOT_INODE};
use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::raw::Request;
use fuse3::{Errno, Result, Timestamp};
use futures_util::stream;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::num::NonZeroU32;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Per-inode cached attributes. Populated by `readdir` (bulk) and `lookup`
/// (single). Per the plan, this is the load-bearing optimisation that
/// prevents `getattr` from RPC-ing on every stat call at 195k-file scale.
#[derive(Debug, Clone)]
struct CachedAttr {
    size: u64,
    mtime_ms: i64,
    is_folder: bool,
    #[allow(dead_code)] // used by Task 2 sub-step 5 (open) and Task 3 (write semantics)
    is_hydrated: bool,
}

impl From<&ListEntry> for CachedAttr {
    fn from(e: &ListEntry) -> Self {
        CachedAttr {
            size: e.size,
            mtime_ms: e.mtime_ms,
            is_folder: e.folder,
            is_hydrated: e.hydrated,
        }
    }
}

/// Per-open-handle state. The FUSE-assigned `fh` (file handle) keys into the
/// `open_handles` map. Each entry carries the cache-file FD we hand reads to
/// plus the JVM-side `handle_id` we generated for the matching `open_read`
/// (used to fire the symmetric `close_handle` on RELEASE).
///
/// Task 3 extends this with `dirty`, `remote_path`, and `cache_path` so the
/// write-side RELEASE can issue `hydration.open_write(handle_id, path,
/// cache_path)` before `close_handle`.
struct OpenHandle {
    file: std::fs::File,
    handle_id: String,
    remote_path: String,
    cache_path: std::path::PathBuf,
    dirty: AtomicBool,
}

/// The unidrive FUSE filesystem.
pub struct UnidriveFs {
    pub(crate) ipc: Arc<Mutex<ReconnectingIpcClient>>,
    pub(crate) paths: Arc<Mutex<PathMap>>,
    /// Inode -> cached attrs. Mirrors what `hydration.list` returned.
    attrs: Arc<Mutex<HashMap<u64, CachedAttr>>>,
    /// FUSE-assigned `fh` -> OpenHandle. Populated in `open`, consumed in
    /// `release`. Reads index by `fh`.
    open_handles: Arc<Mutex<HashMap<u64, OpenHandle>>>,
    /// Monotonic FUSE-side file-handle counter (never zero, so 0 stays
    /// reserved for stateless I/O if we ever need it).
    next_fh: Arc<AtomicU64>,
    /// Monotonic JVM-side handle-id counter. The JVM treats `handle_id` as
    /// an opaque string; we use "rh-<n>" where n is monotonic. (Task 3 will
    /// share this counter for write-side open_write handles.)
    next_handle_id: Arc<AtomicU64>,
    /// Cache root for best-effort cache-file eviction on unlink/rmdir.
    /// `None` in tests that don't exercise eviction; production wires it
    /// from the `--cache` flag via `with_cache_root`.
    cache_root: Option<PathBuf>,
}

impl UnidriveFs {
    pub fn new(ipc: Arc<Mutex<ReconnectingIpcClient>>) -> Self {
        Self {
            ipc,
            paths: Arc::new(Mutex::new(PathMap::new())),
            attrs: Arc::new(Mutex::new(HashMap::new())),
            open_handles: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(AtomicU64::new(1)),
            next_handle_id: Arc::new(AtomicU64::new(1)),
            cache_root: None,
        }
    }

    pub fn with_cache_root(mut self, root: PathBuf) -> Self {
        self.cache_root = Some(root);
        self
    }

    /// Resolve the parent prefix string for a given child remote-path.
    /// "/file.txt" -> "" (root). "/folder/sub" -> "/folder". "" stays "".
    fn parent_prefix(child: &str) -> String {
        match child.rfind('/') {
            None => String::new(),
            Some(0) => String::new(), // "/foo" -> "" (root)
            Some(idx) => child[..idx].to_string(),
        }
    }

    /// Populate the per-inode attrs cache from a freshly-fetched `list`
    /// reply for `parent_prefix`. Also assigns inodes for each entry.
    /// Returns a Vec<(child_inode, ListEntry)> in the order the JVM returned.
    async fn populate_from_list(
        &self,
        parent_prefix: &str,
    ) -> std::result::Result<Vec<(u64, ListEntry)>, IpcError> {
        let entries = {
            let mut ipc = self.ipc.lock().await;
            ipc.list(parent_prefix).await?
        };
        let mut out = Vec::with_capacity(entries.len());
        {
            let mut paths = self.paths.lock().await;
            let mut attrs = self.attrs.lock().await;
            for e in entries {
                let ino = paths.intern(&e.path);
                attrs.insert(ino, CachedAttr::from(&e));
                out.push((ino, e));
            }
        }
        Ok(out)
    }

    async fn get_or_fetch_attr(&self, inode: u64) -> Result<CachedAttr> {
        if inode == ROOT_INODE {
            // Root is always a directory; size/mtime not load-bearing for stat.
            return Ok(CachedAttr {
                size: 0,
                mtime_ms: 0,
                is_folder: true,
                is_hydrated: false,
            });
        }
        // Fast path: cache hit.
        if let Some(a) = self.attrs.lock().await.get(&inode).cloned() {
            return Ok(a);
        }
        // Slow path: someone is statting an inode we don't have cached.
        // Find its parent path and list it to repopulate.
        let path = {
            let paths = self.paths.lock().await;
            paths.path_for(inode).map(|s| s.to_string())
        };
        let path = path.ok_or_else(|| Errno::from(libc::ENOENT))?;
        let parent = Self::parent_prefix(&path);
        self.populate_from_list(&parent)
            .await
            .map_err(ipc_error_to_errno)?;
        self.attrs
            .lock()
            .await
            .get(&inode)
            .cloned()
            .ok_or_else(|| Errno::from(libc::ENOENT))
    }
}

/// Convert a child remote path to a basename `OsStr`-equivalent String.
fn basename(path: &str) -> String {
    match path.rfind('/') {
        None => path.to_string(),
        Some(idx) => path[idx + 1..].to_string(),
    }
}

fn ipc_error_to_errno(e: IpcError) -> Errno {
    match e {
        IpcError::Io(_) => Errno::from(libc::EIO),
        IpcError::Busy => Errno::from(libc::EBUSY),
        IpcError::Malformed(_) | IpcError::ServerError(_) | IpcError::Unknown { .. } => {
            Errno::from(libc::EIO)
        }
    }
}

fn namespace_err_to_errno(e: IpcError) -> Errno {
    match e {
        IpcError::ServerError(ref msg) if msg == "parent_not_found" => Errno::from(libc::ENOENT),
        IpcError::ServerError(ref msg) if msg == "unknown_path" => Errno::from(libc::ENOENT),
        IpcError::ServerError(ref msg) if msg == "path_is_folder" => Errno::from(libc::EISDIR),
        IpcError::ServerError(ref msg) if msg == "path_is_file" => Errno::from(libc::ENOTDIR),
        IpcError::ServerError(ref msg) if msg == "not_empty" => Errno::from(libc::ENOTEMPTY),
        IpcError::ServerError(ref msg) if msg == "path_exists" => Errno::from(libc::EEXIST),
        IpcError::ServerError(ref msg) if msg == "old_path_not_found" => Errno::from(libc::ENOENT),
        IpcError::ServerError(ref msg) if msg == "new_parent_not_found" => Errno::from(libc::ENOENT),
        IpcError::ServerError(ref msg) if msg == "new_path_exists" => Errno::from(libc::EEXIST),
        other => ipc_error_to_errno(other),
    }
}

fn file_attr_from_cached(ino: u64, c: &CachedAttr) -> FileAttr {
    let secs = c.mtime_ms / 1000;
    let nsec = ((c.mtime_ms % 1000) * 1_000_000) as u32;
    let ts = Timestamp::new(secs, nsec);
    let kind = if c.is_folder {
        FileType::Directory
    } else {
        FileType::RegularFile
    };
    // perm: 0o755 for dirs, 0o644 for files. Matches the user-owned model
    // (uid is current user); group/other read-only.
    let perm = if c.is_folder { 0o755 } else { 0o644 };
    FileAttr {
        ino,
        size: c.size,
        blocks: c.size.div_ceil(512),
        atime: ts,
        mtime: ts,
        ctime: ts,
        kind,
        perm,
        nlink: if c.is_folder { 2 } else { 1 },
        // SAFETY: getuid/getgid are pure FFI shims, always safe.
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
    }
}

impl Filesystem for UnidriveFs {
    async fn init(&self, _req: Request) -> Result<ReplyInit> {
        Ok(ReplyInit {
            max_write: NonZeroU32::new(1 << 20).expect("non-zero"),
        })
    }

    async fn destroy(&self, _req: Request) {}

    async fn lookup(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
    ) -> Result<ReplyEntry> {
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;

        // Resolve parent prefix string.
        let parent_path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(parent)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };
        let child_path = if parent_path.is_empty() {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };

        let cached = self.paths.lock().await.inode_for(&child_path);

        if let Some(ino) = cached {
            let a = self.attrs.lock().await.get(&ino).cloned();
            if let Some(a) = a {
                return Ok(ReplyEntry {
                    ttl: Duration::from_secs(1),
                    attr: file_attr_from_cached(ino, &a),
                    generation: 0,
                });
            }
        }

        // Cold path: list parent and find child.
        let listed = self
            .populate_from_list(&parent_path)
            .await
            .map_err(ipc_error_to_errno)?;
        for (ino, e) in &listed {
            if e.path == child_path {
                let a = CachedAttr::from(e);
                return Ok(ReplyEntry {
                    ttl: Duration::from_secs(1),
                    attr: file_attr_from_cached(*ino, &a),
                    generation: 0,
                });
            }
        }
        Err(Errno::from(libc::ENOENT))
    }

    async fn getattr(
        &self,
        _req: Request,
        inode: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Result<ReplyAttr> {
        let a = self.get_or_fetch_attr(inode).await?;
        Ok(ReplyAttr {
            ttl: Duration::from_secs(1),
            attr: file_attr_from_cached(inode, &a),
        })
    }

    async fn opendir(
        &self,
        _req: Request,
        _inode: u64,
        _flags: u32,
    ) -> Result<ReplyOpen> {
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }

    async fn open(&self, _req: Request, inode: u64, flags: u32) -> Result<ReplyOpen> {
        // Resolve inode -> remote path.
        let path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(inode)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };
        if path.is_empty() {
            // Can't open the root as a file.
            return Err(Errno::from(libc::EISDIR));
        }

        // Decide read-only vs write-capable. The kernel may pass O_RDONLY,
        // O_WRONLY, or O_RDWR in the access-mode bits (O_ACCMODE). The JVM
        // treats handle_id as opaque; we use distinct prefixes purely as a
        // debugging aid.
        //
        // For O_TRUNC write-opens: call open_write_begin (prepares an empty
        // cache without downloading the existing content). The handle starts
        // dirty=true so a zero-write release still commits the empty file.
        // The handle_id is passed to open_write_begin so the JVM registers a
        // live open-set entry; release fires the symmetric close_handle.
        //
        // For all other write-opens: call open_read so the JVM materialises
        // the cache file before we write into it.
        let acc = (flags as i32) & libc::O_ACCMODE;
        let writable = acc == libc::O_WRONLY || acc == libc::O_RDWR;
        let truncating = writable && (flags as i32) & libc::O_TRUNC != 0;
        let handle_id = if writable {
            format!("wh-{}", self.next_handle_id.fetch_add(1, Ordering::Relaxed))
        } else {
            format!("rh-{}", self.next_handle_id.fetch_add(1, Ordering::Relaxed))
        };

        let cache_path = if truncating {
            // Pass handle_id so the JVM registers a live open-set entry for this
            // O_TRUNC open.  release() will call close_handle(handle_id) for
            // symmetric cleanup — the handle_id is already in the OpenHandle.
            self.ipc.lock().await.open_write_begin(&path, Some(&handle_id)).await
                .map_err(namespace_err_to_errno)?
                .cache_path
        } else {
            // open_read drives the JVM-side hydrate-on-cache-miss download. A
            // failure here (expired auth, 404, network) returns an IpcError that
            // ipc_error_to_errno otherwise collapses to a bare EIO with no
            // breadcrumb — the original read-path EIO had no log line at all.
            // Log the actual variant/message before mapping so the EIO is
            // diagnosable and the JVM-side vs co-daemon-side split is obvious.
            let reply = self.ipc.lock().await.open_read(&handle_id, &path).await
                .map_err(|e| {
                    tracing::warn!(error=%e, path=%path, handle_id=%handle_id, "open_read failed");
                    ipc_error_to_errno(e)
                })?;
            reply.cache_path
        };

        // Open the cache file at the path the JVM returned.
        let file = if writable {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&cache_path)
        } else {
            std::fs::File::open(&cache_path)
        }
        .map_err(|e| {
            tracing::warn!(?e, cache_path=%cache_path.display(), "open(cache_path) failed");
            Errno::from(libc::EIO)
        })?;

        // O_TRUNC opens start dirty=true: the file is empty from the moment
        // open_write_begin returned, so even a zero-write close must commit.
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.open_handles.lock().await.insert(
            fh,
            OpenHandle {
                file,
                handle_id,
                remote_path: path,
                cache_path,
                dirty: AtomicBool::new(truncating),
            },
        );
        Ok(ReplyOpen { fh, flags: 0 })
    }

    async fn read(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<ReplyData> {
        let handles = self.open_handles.lock().await;
        let h = handles.get(&fh).ok_or_else(|| Errno::from(libc::EBADF))?;
        let mut buf = vec![0u8; size as usize];
        // SAFETY: pread is safe; std::os::unix::fs::FileExt::read_at maps to it.
        let n = h
            .file
            .read_at(&mut buf, offset)
            .map_err(|_| Errno::from(libc::EIO))?;
        buf.truncate(n);
        Ok(ReplyData {
            data: Bytes::from(buf),
        })
    }

    async fn write(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> Result<ReplyWrite> {
        let handles = self.open_handles.lock().await;
        let h = handles.get(&fh).ok_or_else(|| Errno::from(libc::EBADF))?;
        // SAFETY: write_at maps to pwrite; safe.
        let n = h
            .file
            .write_at(data, offset)
            .map_err(|_| Errno::from(libc::EIO))?;
        h.dirty.store(true, Ordering::Relaxed);
        Ok(ReplyWrite { written: n as u32 })
    }

    async fn fsync(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        datasync: bool,
    ) -> Result<()> {
        // fsync flushes the cache FD, then — if the handle is dirty —
        // synchronously commits the bytes to the cloud and AWAITS the
        // result. An app that explicitly fsync()s is asking for a durability
        // guarantee; flushing the local FD alone gives it none, since the
        // cloud upload otherwise only fires at RELEASE. Unlike RELEASE
        // (where close(2) has already returned and the kernel ignores the
        // error code), fsync CAN return an errno to userland, so an upload
        // failure surfaces as a failed fsync rather than vanishing.
        //
        // Clone the handle's identity out from under the open_handles lock
        // before the IPC await — mirrors RELEASE, which never holds the map
        // lock across an ipc round-trip.
        // CLAIM the dirty bit with swap(false) BEFORE the upload, not a
        // load-then-clear-after. A write() racing this fsync (concurrent
        // threads sharing the fd, or multiple in-flight FUSE ops on the same
        // handle) sets dirty=true; if we cleared dirty *after* the await we'd
        // clobber that, and RELEASE would skip the later bytes — fsync would
        // have ack'd durability for data that never reaches the cloud. By
        // swapping to false up front, any racing write re-sets dirty=true
        // after our swap, so RELEASE still re-commits its bytes. We never
        // clear dirty again on the success path.
        let (handle_id, remote_path, cache_path, was_dirty) = {
            let handles = self.open_handles.lock().await;
            let h = handles.get(&fh).ok_or_else(|| Errno::from(libc::EBADF))?;
            if datasync {
                h.file.sync_data().map_err(|_| Errno::from(libc::EIO))?;
            } else {
                h.file.sync_all().map_err(|_| Errno::from(libc::EIO))?;
            }
            (
                h.handle_id.clone(),
                h.remote_path.clone(),
                h.cache_path.clone(),
                h.dirty.swap(false, Ordering::AcqRel),
            )
        };
        if !was_dirty {
            return Ok(());
        }
        let cache_path_str = cache_path.to_string_lossy();
        let upload = {
            let mut ipc = self.ipc.lock().await;
            ipc.open_write(&handle_id, &remote_path, &cache_path_str)
                .await
        };
        if let Err(e) = upload {
            // Upload failed: restore the dirty bit so RELEASE retries the
            // commit. store(true) is idempotent against a concurrent write
            // that already re-set it. Re-fetch under the lock — the fh may
            // have been released during the await (then there's nothing to
            // retry and the error still surfaces to the failed fsync).
            if let Some(h) = self.open_handles.lock().await.get(&fh) {
                h.dirty.store(true, Ordering::Release);
            }
            return Err(ipc_error_to_errno(e));
        }
        // Success: do NOT touch dirty. If a write raced the upload it set
        // dirty=true after our swap, and RELEASE must re-commit those bytes.
        Ok(())
    }

    async fn release(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> Result<()> {
        // Drop the cache-file FD, then fire (if dirty) open_write to the JVM,
        // then close_handle. Every JVM-registered open (open_read, create, and
        // now O_TRUNC open_write_begin with handle_id) must be matched by a
        // close_handle. For the no-fh setattr/bare-truncate path,
        // open_write_begin is called without a handle_id (no open-set entry),
        // but that path never reaches here (there is no FUSE fh to release).
        // A dirty-release must fire open_write FIRST so the JVM sees the upload
        // trigger before it learns the handle has been released.
        let removed = self.open_handles.lock().await.remove(&fh);
        let Some(h) = removed else {
            // RELEASE for an unknown fh — treat as no-op rather than error;
            // the kernel doesn't read the error code anyway (per fuse3 docs).
            return Ok(());
        };
        let was_dirty = h.dirty.load(Ordering::Relaxed);
        // Drop the FD before issuing IPC — the JVM may want to read the
        // cache file itself to upload.
        drop(h.file);
        if was_dirty {
            let cache_path_str = h.cache_path.to_string_lossy();
            if let Err(e) = {
                let mut ipc = self.ipc.lock().await;
                ipc.open_write(&h.handle_id, &h.remote_path, &cache_path_str).await
            } {
                tracing::warn!(
                    ?e,
                    handle_id=%h.handle_id,
                    path=%h.remote_path,
                    "open_write IPC failed on dirty release"
                );
            }
        }
        // close_handle errors are logged but not surfaced — the user's
        // close(2) has already happened.
        if let Err(e) = {
            let mut ipc = self.ipc.lock().await;
            ipc.close_handle(&h.handle_id).await
        } {
            tracing::warn!(?e, handle_id=%h.handle_id, "close_handle IPC failed");
        }
        Ok(())
    }

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        parent: u64,
        _fh: u64,
        offset: i64,
    ) -> Result<ReplyDirectory<impl futures_util::stream::Stream<Item = Result<DirectoryEntry>> + Send + 'a>>
    {
        let parent_path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(parent)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };

        let listed = self
            .populate_from_list(&parent_path)
            .await
            .map_err(ipc_error_to_errno)?;

        // Build the entry list: "." (self), ".." (parent), then real entries.
        // offsets are 1-indexed and point to the _next_ entry (per fuse3 docs).
        let mut all: Vec<DirectoryEntry> = Vec::with_capacity(listed.len() + 2);
        all.push(DirectoryEntry {
            inode: parent,
            kind: FileType::Directory,
            name: ".".into(),
            offset: 1,
        });
        all.push(DirectoryEntry {
            inode: parent, // ".." would normally be parent's parent; we
            // don't track parent-of-parent, so pointing at self is the
            // standard behaviour for the root and an acceptable cheat for
            // non-root (the kernel does not enforce ".." inode value for
            // unprivileged mounts).
            kind: FileType::Directory,
            name: "..".into(),
            offset: 2,
        });
        for (i, (ino, e)) in listed.iter().enumerate() {
            let kind = if e.folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all.push(DirectoryEntry {
                inode: *ino,
                kind,
                name: basename(&e.path).into(),
                offset: (i as i64) + 3,
            });
        }

        let skip = offset.max(0) as usize;
        let drained: Vec<Result<DirectoryEntry>> =
            all.into_iter().skip(skip).map(Ok).collect();
        Ok(ReplyDirectory {
            entries: stream::iter(drained),
        })
    }

    /// readdirplus is preferred by modern kernels (FUSE_DO_READDIRPLUS) because
    /// it combines readdir + lookup into one round-trip. The fuse3 default
    /// returns ENOSYS, which surfaces to `ls` as "general io error". We
    /// implement it by reusing the same `populate_from_list` path then
    /// emitting DirectoryEntryPlus with full attrs.
    async fn readdirplus<'a>(
        &'a self,
        _req: Request,
        parent: u64,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> Result<ReplyDirectoryPlus<impl futures_util::stream::Stream<Item = Result<DirectoryEntryPlus>> + Send + 'a>>
    {
        let parent_path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(parent)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };

        let listed = self
            .populate_from_list(&parent_path)
            .await
            .map_err(ipc_error_to_errno)?;

        let parent_attr = self.get_or_fetch_attr(parent).await?;
        let parent_file_attr = file_attr_from_cached(parent, &parent_attr);

        let mut all: Vec<DirectoryEntryPlus> = Vec::with_capacity(listed.len() + 2);
        all.push(DirectoryEntryPlus {
            inode: parent,
            generation: 0,
            kind: FileType::Directory,
            name: ".".into(),
            offset: 1,
            attr: parent_file_attr,
            entry_ttl: Duration::from_secs(1),
            attr_ttl: Duration::from_secs(1),
        });
        all.push(DirectoryEntryPlus {
            inode: parent,
            generation: 0,
            kind: FileType::Directory,
            name: "..".into(),
            offset: 2,
            attr: parent_file_attr,
            entry_ttl: Duration::from_secs(1),
            attr_ttl: Duration::from_secs(1),
        });
        for (i, (ino, e)) in listed.iter().enumerate() {
            let attr = file_attr_from_cached(*ino, &CachedAttr::from(e));
            let kind = if e.folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all.push(DirectoryEntryPlus {
                inode: *ino,
                generation: 0,
                kind,
                name: basename(&e.path).into(),
                offset: (i as i64) + 3,
                attr,
                entry_ttl: Duration::from_secs(1),
                attr_ttl: Duration::from_secs(1),
            });
        }

        let skip = offset as usize;
        let drained: Vec<Result<DirectoryEntryPlus>> =
            all.into_iter().skip(skip).map(Ok).collect();
        Ok(ReplyDirectoryPlus {
            entries: stream::iter(drained),
        })
    }

    async fn mkdir(
        &self,
        _req: Request,
        parent_inode: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
    ) -> Result<ReplyEntry> {
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let parent_path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(parent_inode)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };
        let child_path = format!("{}/{}",
            parent_path.trim_end_matches('/'),
            name,
        );
        {
            let mut ipc = self.ipc.lock().await;
            ipc.mkdir(&child_path).await.map_err(namespace_err_to_errno)?;
        }
        let new_ino = {
            let mut paths = self.paths.lock().await;
            paths.intern(&child_path)
        };
        let mtime_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let cached = CachedAttr {
            size: 0,
            mtime_ms,
            is_folder: true,
            is_hydrated: false,
        };
        let attr = file_attr_from_cached(new_ino, &cached);
        self.attrs.lock().await.insert(new_ino, cached);
        Ok(ReplyEntry { ttl: Duration::from_secs(1), attr, generation: 0 })
    }

    async fn unlink(
        &self,
        _req: Request,
        parent_inode: u64,
        name: &OsStr,
    ) -> Result<()> {
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let parent_path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(parent_inode)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };
        let child_path = format!("{}/{}",
            parent_path.trim_end_matches('/'),
            name,
        );
        {
            let mut ipc = self.ipc.lock().await;
            ipc.unlink(&child_path).await.map_err(namespace_err_to_errno)?;
        }
        // Drop the path-map entry AND the attrs cache entry for the
        // deleted path. Without the path-map drop, mkdir/rm churn in a
        // long-lived mount grows PathMap monotonically.
        let ino = {
            let mut paths = self.paths.lock().await;
            paths.forget(&child_path)
        };
        if let Some(inode) = ino {
            self.attrs.lock().await.remove(&inode);
        }
        // Best-effort: NotFound is the expected case for never-hydrated
        // paths and must not surface; any other I/O error is logged but
        // does not fail the unlink (cloud copy and state.db row are
        // already gone, per spec §4 R5 / NG4).
        if let Some(root) = &self.cache_root {
            let rel = child_path.trim_start_matches('/');
            let cache_path = root.join(rel);
            if let Err(e) = std::fs::remove_file(&cache_path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(?e, ?cache_path, "cache file eviction failed on unlink");
                }
            }
        }
        Ok(())
    }

    async fn rmdir(
        &self,
        _req: Request,
        parent_inode: u64,
        name: &OsStr,
    ) -> Result<()> {
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let parent_path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(parent_inode)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };
        let child_path = format!("{}/{}",
            parent_path.trim_end_matches('/'),
            name,
        );
        {
            let mut ipc = self.ipc.lock().await;
            ipc.rmdir(&child_path).await.map_err(namespace_err_to_errno)?;
        }
        let ino = {
            let mut paths = self.paths.lock().await;
            paths.forget(&child_path)
        };
        if let Some(inode) = ino {
            self.attrs.lock().await.remove(&inode);
        }
        // Best-effort cache eviction; see unlink for the rationale. Folders
        // may contain hydrated children, so remove_dir_all not remove_file.
        if let Some(root) = &self.cache_root {
            let rel = child_path.trim_start_matches('/');
            let cache_path = root.join(rel);
            if let Err(e) = std::fs::remove_dir_all(&cache_path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(?e, ?cache_path, "cache dir eviction failed on rmdir");
                }
            }
        }
        Ok(())
    }

    async fn create(
        &self,
        _req: Request,
        parent_inode: u64,
        name: &OsStr,
        _mode: u32,
        _flags: u32,
    ) -> Result<ReplyCreated> {
        let (fh, attr, new_ino) = self.create_internal(parent_inode, name).await?;
        let _ = new_ino;
        Ok(ReplyCreated {
            ttl: Duration::from_secs(1),
            attr,
            generation: 0,
            fh,
            flags: 0,
        })
    }

    async fn mknod(
        &self,
        _req: Request,
        parent_inode: u64,
        name: &OsStr,
        mode: u32,
        _rdev: u32,
    ) -> Result<ReplyEntry> {
        // mknod is the legacy fallback some kernels still emit for O_CREAT
        // on pre-FUSE-7.23 filesystems. We're a cloud filesystem, so we only
        // honour regular files; character/block/fifo/socket nodes are EPERM.
        // S_IFMT extracts the file-type bits; S_IFREG is the regular-file
        // marker. Per POSIX, mode==0 (no type bits set) is implementation-
        // defined — treat it as a regular-file request to match how Linux's
        // sys_mknodat handles bare 0644 modes.
        let typ = (mode as libc::mode_t) & libc::S_IFMT;
        if typ != 0 && typ != libc::S_IFREG {
            return Err(Errno::from(libc::EPERM));
        }
        let (fh, attr, _new_ino) = self.create_internal(parent_inode, name).await?;
        // mknod doesn't return a file handle; release the synthetic one we
        // allocated so we don't leak an open_handles entry. The kernel will
        // call open() separately when the caller actually wants to write.
        let removed = self.open_handles.lock().await.remove(&fh);
        if let Some(h) = removed {
            // Drop the cache FD and fire the upload-on-RELEASE path so the
            // empty file ends up on the cloud — the kernel won't follow up
            // with a release for this fh since we never handed it back.
            let cache_path_str = h.cache_path.to_string_lossy().to_string();
            drop(h.file);
            if let Err(e) = {
                let mut ipc = self.ipc.lock().await;
                ipc.open_write(&h.handle_id, &h.remote_path, &cache_path_str).await
            } {
                tracing::warn!(
                    ?e,
                    handle_id=%h.handle_id,
                    path=%h.remote_path,
                    "open_write IPC failed on mknod-synthetic release"
                );
            }
            if let Err(e) = {
                let mut ipc = self.ipc.lock().await;
                ipc.close_handle(&h.handle_id).await
            } {
                tracing::warn!(?e, handle_id=%h.handle_id, "close_handle IPC failed on mknod");
            }
        }
        Ok(ReplyEntry { ttl: Duration::from_secs(1), attr, generation: 0 })
    }

    async fn setattr(
        &self,
        _req: Request,
        inode: u64,
        fh: Option<u64>,
        set_attr: SetAttr,
    ) -> Result<ReplyAttr> {
        // ----------------------------------------------------------------
        // Resolve the remote path for this inode.
        // ----------------------------------------------------------------
        let path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(inode)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };

        // ----------------------------------------------------------------
        // Handle size truncation (highest priority; mode/uid/gid are
        // no-ops and are handled later).
        // ----------------------------------------------------------------
        if let Some(new_size) = set_attr.size {
            if new_size == 0 {
                // ---- truncate to zero ----
                // For a live open handle: truncate its already-materialised
                // cache file and mark dirty (commit deferred to release/fsync).
                // For a bare truncate (no fh): call open_write_begin so the
                // JVM materialises an empty cache file, then commit
                // synchronously (mirrors fsync's dirty-claim discipline).
                let cache_path = if let Some(fh_val) = fh {
                    // fh path — use the existing cache file.
                    let handles = self.open_handles.lock().await;
                    if let Some(h) = handles.get(&fh_val) {
                        // Truncate to 0 BEFORE marking dirty so a failed
                        // truncate never contaminates the dirty-commit path.
                        h.file
                            .set_len(0)
                            .map_err(|_| Errno::from(libc::EIO))?;
                        h.dirty.store(true, Ordering::Release);
                        None // no synchronous commit needed
                    } else {
                        return Err(Errno::from(libc::EBADF));
                    }
                } else {
                    // Bare truncate — JVM materialises an empty cache file via
                    // open_write_begin (prepareEmptyCache uses TRUNCATE_EXISTING).
                    // No handle_id: this is a one-shot operation; the JVM must NOT
                    // register an open-set entry (there is no release to close it).
                    let reply = {
                        let mut ipc = self.ipc.lock().await;
                        ipc.open_write_begin(&path, None).await
                    }
                    .map_err(namespace_err_to_errno)?;
                    Some(reply.cache_path)
                };

                // Synchronous commit for the no-fh path.
                if let Some(cp) = cache_path {
                    let cache_path_str = cp.to_string_lossy();
                    // Allocate a synthetic handle_id; the JVM's openForWrite
                    // registers it in openSets[connectionId][handleId].  We must
                    // call close_handle after open_write — whether or not it
                    // succeeded — to release that entry (mirrors the size=N path's
                    // "always close" pattern).  Note: open_write_begin above was
                    // called with None (no handle_id), so it registers nothing and
                    // needs no close.  Only this open_write call registers a handle.
                    let handle_id = format!(
                        "trunc0-{}",
                        self.next_handle_id.fetch_add(1, Ordering::Relaxed)
                    );
                    let upload = {
                        let mut ipc = self.ipc.lock().await;
                        ipc.open_write(&handle_id, &path, &cache_path_str)
                            .await
                    };
                    // Always release the JVM open-set entry — even if open_write
                    // failed — to avoid leaking the handle registered by open_write.
                    {
                        let mut ipc = self.ipc.lock().await;
                        let _ = ipc.close_handle(&handle_id).await;
                    }
                    upload.map_err(ipc_error_to_errno)?;
                }

                // Update attrs cache.
                let mut attrs = self.attrs.lock().await;
                if let Some(a) = attrs.get_mut(&inode) {
                    a.size = 0;
                }
            } else {
                // ---- truncate to N > 0 ----
                // For a live open handle: set_len on the existing cache file.
                // For a bare truncate: download via open_read first.
                if let Some(fh_val) = fh {
                    let handles = self.open_handles.lock().await;
                    if let Some(h) = handles.get(&fh_val) {
                        // set_len BEFORE dirty mark — failure must not commit.
                        h.file
                            .set_len(new_size)
                            .map_err(|_| Errno::from(libc::EIO))?;
                        h.dirty.store(true, Ordering::Release);
                    } else {
                        return Err(Errno::from(libc::EBADF));
                    }
                } else {
                    // Bare truncate — download first.
                    let handle_id = format!(
                        "truncN-{}",
                        self.next_handle_id.fetch_add(1, Ordering::Relaxed)
                    );
                    let reply = {
                        let mut ipc = self.ipc.lock().await;
                        ipc.open_read(&handle_id, &path).await
                    }
                    .map_err(ipc_error_to_errno)?;

                    // Open the cache file for writing, then set_len.
                    // IMPORTANT: set_len BEFORE any dirty-mark or open_write.
                    // On failure: close_handle to avoid leaking the JVM-side
                    // open-set entry, then return EIO without committing.
                    let open_result = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&reply.cache_path);
                    let file = match open_result {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!(?e, cache_path=%reply.cache_path.display(),
                                "setattr: open(cache_path) failed for truncate-to-N");
                            // Release the JVM open-set entry; ignore errors
                            // (we're already on the failure path).
                            let _ = {
                                let mut ipc = self.ipc.lock().await;
                                ipc.close_handle(&handle_id).await
                            };
                            return Err(Errno::from(libc::EIO));
                        }
                    };
                    if let Err(e) = file.set_len(new_size) {
                        tracing::warn!(?e, new_size, "setattr: set_len failed");
                        let _ = {
                            let mut ipc = self.ipc.lock().await;
                            ipc.close_handle(&handle_id).await
                        };
                        return Err(Errno::from(libc::EIO));
                    }
                    // set_len succeeded: commit synchronously.
                    drop(file);
                    let cache_path_str = reply.cache_path.to_string_lossy();
                    let upload = {
                        let mut ipc = self.ipc.lock().await;
                        ipc.open_write(&handle_id, &path, &cache_path_str).await
                    };
                    // Always release the JVM open-set entry — even if open_write
                    // failed — to avoid leaking the handle registered by open_read.
                    {
                        let mut ipc = self.ipc.lock().await;
                        let _ = ipc.close_handle(&handle_id).await;
                    }
                    upload.map_err(ipc_error_to_errno)?;
                }

                // Update attrs cache.
                let mut attrs = self.attrs.lock().await;
                if let Some(a) = attrs.get_mut(&inode) {
                    a.size = new_size;
                }
            }
        }

        // ----------------------------------------------------------------
        // mode / uid / gid — cloud has no POSIX mode; accept as no-op.
        // ----------------------------------------------------------------
        // (Nothing to do; we simply don't return an error.)

        // ----------------------------------------------------------------
        // atime / mtime — best-effort.
        // `filetime` is not a dependency; skip silently.
        // To add support: add filetime = "0.2" to Cargo.toml and use
        // filetime::set_file_times(cache_path, atime, mtime).
        // ----------------------------------------------------------------

        // ----------------------------------------------------------------
        // Return refreshed attrs.
        // ----------------------------------------------------------------
        let a = self.get_or_fetch_attr(inode).await?;
        Ok(ReplyAttr {
            ttl: Duration::from_secs(1),
            attr: file_attr_from_cached(inode, &a),
        })
    }

    async fn statfs(&self, _req: Request, _inode: u64) -> Result<ReplyStatFs> {
        // Static reply: no IPC needed (backlog §statfs). Values represent a
        // large, mostly-free cloud volume so df/file-managers see sane output.
        // Block size 4 KiB; total ≈16 TiB (1<<32 blocks × 4 KiB).
        const BLOCKS: u64 = 1 << 32;
        Ok(ReplyStatFs {
            blocks: BLOCKS,
            bfree: BLOCKS,
            bavail: BLOCKS,
            files: 1 << 20,
            ffree: 1 << 20,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        })
    }

    /// Extended attribute stubs. Cloud storage has no xattr store, so we
    /// return the semantically-correct "no such attribute" / "not supported"
    /// errors rather than falling through to the fuse3 default ENOSYS, which
    /// causes desktop stacks (KDE/GNOME, ACL queries) to log errors or skip
    /// files.
    async fn getxattr(
        &self,
        _req: Request,
        _inode: u64,
        _name: &OsStr,
        _size: u32,
    ) -> Result<ReplyXAttr> {
        Err(Errno::from(libc::ENODATA))
    }

    async fn listxattr(&self, _req: Request, _inode: u64, size: u32) -> Result<ReplyXAttr> {
        // An empty xattr list: 0 bytes needed. When size == 0 the caller is
        // doing a size probe — reply with Size(0). When size > 0 the caller
        // wants the data — reply with an empty buffer.
        if size == 0 {
            Ok(ReplyXAttr::Size(0))
        } else {
            Ok(ReplyXAttr::Data(Bytes::new()))
        }
    }

    async fn setxattr(
        &self,
        _req: Request,
        _inode: u64,
        _name: &OsStr,
        _value: &[u8],
        _flags: u32,
        _position: u32,
    ) -> Result<()> {
        Err(Errno::from(libc::EOPNOTSUPP))
    }

    async fn removexattr(&self, _req: Request, _inode: u64, _name: &OsStr) -> Result<()> {
        Err(Errno::from(libc::ENODATA))
    }

    async fn rename(
        &self,
        _req: Request,
        old_parent_inode: u64,
        old_name: &OsStr,
        new_parent_inode: u64,
        new_name: &OsStr,
    ) -> Result<()> {
        let old_name = old_name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let new_name = new_name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;

        // Resolve both parent inodes -> cloud-path strings in a single lock
        // acquisition so a concurrent forget/intern can't slip between.
        let (old_path, new_path) = {
            let paths = self.paths.lock().await;
            let old_parent = paths
                .path_for(old_parent_inode)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?;
            let new_parent = paths
                .path_for(new_parent_inode)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?;
            let old_path = format!("{}/{}", old_parent.trim_end_matches('/'), old_name);
            let new_path = format!("{}/{}", new_parent.trim_end_matches('/'), new_name);
            (old_path, new_path)
        };

        // Wire to the JVM. The JVM pre-flights source-exists,
        // destination-parent-exists, destination-doesn't-exist and emits
        // typed wire errors that namespace_err_to_errno maps below to
        // ENOENT/ENOENT/EEXIST. No POSIX-overwrite semantics: providers
        // (OneDrive PATCH, Internxt move) do not support atomic replace,
        // and emulating it via delete-then-rename leaves a window where
        // the destination is missing. Userland unlink-then-rename works.
        {
            let mut ipc = self.ipc.lock().await;
            ipc.rename(&old_path, &new_path).await.map_err(namespace_err_to_errno)?;
        }

        // PathMap update: preserve inode across rename (POSIX semantics).
        // For folder renames, descendant inodes stay where they are — their
        // string keys in the path map become stale until the next lookup.
        // That's acceptable: stale entries only matter if a kernel-cached
        // inode is reused, and the kernel's dentry cache invalidates
        // descendants on a parent rename. The JVM-side state.db update
        // (via renamePrefix) is the authoritative source of truth.
        {
            let mut paths = self.paths.lock().await;
            paths.rename(&old_path, &new_path);
        }

        Ok(())
    }
}

impl UnidriveFs {
    /// Shared body for FUSE `create` and `mknod` — issues hydration.create
    /// to the JVM, opens the resulting cache file for read+write, allocates
    /// an OpenHandle, and returns the FUSE fh + attrs. The handle is marked
    /// `dirty=true` from the start: POSIX `touch foo` creates a file
    /// without writing to it, and the user expects an empty file to land
    /// on the cloud. Without the create-time dirty bit, a zero-write
    /// release would skip the hydration.open_write upload trigger.
    async fn create_internal(
        &self,
        parent_inode: u64,
        name: &OsStr,
    ) -> Result<(u64, FileAttr, u64)> {
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let parent_path = {
            let paths = self.paths.lock().await;
            paths
                .path_for(parent_inode)
                .map(|s| s.to_string())
                .ok_or_else(|| Errno::from(libc::ENOENT))?
        };
        let child_path = format!(
            "{}/{}",
            parent_path.trim_end_matches('/'),
            name,
        );

        let handle_id = format!("create-{}", self.next_handle_id.fetch_add(1, Ordering::Relaxed));
        let reply = {
            let mut ipc = self.ipc.lock().await;
            ipc.create(&handle_id, &child_path).await
        }
        .map_err(namespace_err_to_errno)?;

        // Open the cache file RW; the JVM already created + truncated it,
        // but match the kernel's O_CREAT|O_TRUNC semantics so a stray write
        // by another caller (shouldn't happen — JVM holds the open-set
        // record) can't observe stale bytes.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&reply.cache_path)
            .map_err(|e| {
                tracing::warn!(?e, cache_path=%reply.cache_path.display(), "open(cache_path) failed in create");
                Errno::from(libc::EIO)
            })?;

        let new_ino = {
            let mut paths = self.paths.lock().await;
            paths.intern(&child_path)
        };
        let mtime_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let cached = CachedAttr {
            size: 0,
            mtime_ms,
            is_folder: false,
            is_hydrated: true,
        };
        let attr = file_attr_from_cached(new_ino, &cached);
        self.attrs.lock().await.insert(new_ino, cached);

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.open_handles.lock().await.insert(
            fh,
            OpenHandle {
                file,
                handle_id: reply.handle_id,
                remote_path: child_path,
                cache_path: reply.cache_path,
                // dirty=true from creation: see method docstring.
                dirty: AtomicBool::new(true),
            },
        );
        Ok((fh, attr, new_ino))
    }
}

