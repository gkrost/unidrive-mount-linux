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
}
