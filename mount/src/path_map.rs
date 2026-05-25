use std::collections::HashMap;

/// FUSE protocol reserves inode `1` for the filesystem root.
pub const ROOT_INODE: u64 = 1;

/// Bidirectional inode <-> path map. Inodes for non-root entries are assigned
/// monotonically from `ROOT_INODE + 1`. Re-interning the same path returns
/// the previously assigned inode.
///
/// Root convention: the root inode (`1`) maps to the empty string `""`. The
/// JVM-side `hydration.list("")` returns the cloud root's direct children
/// (verified against `HydrationIpcHandler.kt` — `list` takes a prefix string,
/// empty means root). Therefore `""` is the canonical root remote-path.
pub struct PathMap {
    next: u64,
    path_to_inode: HashMap<String, u64>,
    inode_to_path: HashMap<u64, String>,
}

impl PathMap {
    pub fn new() -> Self {
        let mut m = PathMap {
            next: ROOT_INODE + 1,
            path_to_inode: HashMap::new(),
            inode_to_path: HashMap::new(),
        };
        // Pre-register the root mapping.
        m.path_to_inode.insert(String::new(), ROOT_INODE);
        m.inode_to_path.insert(ROOT_INODE, String::new());
        m
    }

    pub fn root_inode(&self) -> u64 {
        ROOT_INODE
    }

    /// Return the inode for `path`, assigning a new one if not yet seen.
    pub fn intern(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.path_to_inode.get(path) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.path_to_inode.insert(path.to_string(), ino);
        self.inode_to_path.insert(ino, path.to_string());
        ino
    }

    pub fn path_for(&self, inode: u64) -> Option<&str> {
        self.inode_to_path.get(&inode).map(|s| s.as_str())
    }

    pub fn inode_for(&self, path: &str) -> Option<u64> {
        self.path_to_inode.get(path).copied()
    }

    /// Drop both halves of the mapping for `path`. Returns the inode that
    /// was released, or `None` if the path was not interned. The inode
    /// number itself is not recycled — `next` keeps advancing — but the
    /// HashMap entries are reclaimed, so create/delete churn in a long-
    /// lived mount no longer grows the map unboundedly.
    pub fn forget(&mut self, path: &str) -> Option<u64> {
        let ino = self.path_to_inode.remove(path)?;
        self.inode_to_path.remove(&ino);
        Some(ino)
    }
}

impl Default for PathMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_inode_is_one() {
        let m = PathMap::new();
        assert_eq!(m.root_inode(), 1);
    }

    #[test]
    fn root_maps_to_empty_path() {
        let m = PathMap::new();
        assert_eq!(m.path_for(ROOT_INODE), Some(""));
    }

    #[test]
    fn empty_path_maps_back_to_root_on_intern() {
        let mut m = PathMap::new();
        assert_eq!(m.intern(""), ROOT_INODE);
    }

    #[test]
    fn interning_same_path_returns_same_inode() {
        let mut m = PathMap::new();
        let a = m.intern("/foo.txt");
        let b = m.intern("/foo.txt");
        assert_eq!(a, b);
    }

    #[test]
    fn different_paths_get_different_inodes() {
        let mut m = PathMap::new();
        let a = m.intern("/foo.txt");
        let b = m.intern("/bar.txt");
        assert_ne!(a, b);
    }

    #[test]
    fn path_for_returns_the_original_path() {
        let mut m = PathMap::new();
        let ino = m.intern("/folder/deep/file.txt");
        assert_eq!(m.path_for(ino), Some("/folder/deep/file.txt"));
    }

    #[test]
    fn path_for_unknown_inode_returns_none() {
        let m = PathMap::new();
        assert_eq!(m.path_for(9999), None);
    }

    #[test]
    fn new_inodes_start_above_root() {
        let mut m = PathMap::new();
        let first = m.intern("/a");
        assert!(first > ROOT_INODE);
    }

    #[test]
    fn inode_for_returns_none_for_unknown_path() {
        let mut map = PathMap::new();
        let known_ino = map.intern("/known");
        assert_eq!(map.inode_for("/known"), Some(known_ino),
            "interned path must reverse-lookup to its inode");
        assert_eq!(map.inode_for("/never_interned"), None,
            "unknown path must yield None, not a sentinel or panic");
    }

    #[test]
    fn forget_releases_both_halves_and_returns_freed_inode() {
        let mut map = PathMap::new();
        let ino = map.intern("/doomed");
        assert_eq!(map.forget("/doomed"), Some(ino),
            "forget must return the inode it released");
        assert_eq!(map.inode_for("/doomed"), None,
            "path->inode entry must be gone after forget");
        assert_eq!(map.path_for(ino), None,
            "inode->path entry must be gone after forget");
        assert_eq!(map.forget("/doomed"), None,
            "second forget on the same path must yield None, not panic");
    }

    #[test]
    fn forget_then_reintern_assigns_a_fresh_inode() {
        // Inode numbers MUST NOT be recycled — a kernel-cached dentry
        // pointing at the old inode must not collide with a re-created
        // path's inode (would surface as ghost-content for the new file).
        let mut map = PathMap::new();
        let first = map.intern("/recreate");
        map.forget("/recreate");
        let second = map.intern("/recreate");
        assert_ne!(first, second,
            "re-interning after forget must allocate a new inode, not reuse");
    }
}
