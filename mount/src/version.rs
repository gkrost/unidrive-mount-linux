// The version `--version` prints: the release version for a tagged, clean
// build; otherwise the release version plus the commit id as semver build
// metadata, `+<commit>` or `+<commit>.dirty` (build.rs decides which).

pub fn compose(version: &str, commit: &str, dirty: bool, tagged: bool) -> String {
    if tagged {
        version.to_string()
    } else if dirty {
        format!("{version}+{commit}.dirty")
    } else {
        format!("{version}+{commit}")
    }
}

pub fn version_string() -> String {
    compose(
        env!("CARGO_PKG_VERSION"),
        env!("UNIDRIVE_MOUNT_COMMIT"),
        env!("UNIDRIVE_MOUNT_DIRTY") == "true",
        env!("UNIDRIVE_MOUNT_TAGGED") == "true",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_build_prints_the_bare_version() {
        assert_eq!(compose("0.0.2", "abc1234", false, true), "0.0.2");
    }

    #[test]
    fn any_other_build_carries_the_commit() {
        assert_eq!(compose("0.0.2", "abc1234", false, false), "0.0.2+abc1234");
    }

    #[test]
    fn an_uncommitted_build_says_so() {
        assert_eq!(compose("0.0.2", "abc1234", true, false), "0.0.2+abc1234.dirty");
    }

    #[test]
    fn the_release_part_leads_whatever_follows() {
        let v = version_string();
        assert!(v.starts_with(env!("CARGO_PKG_VERSION")), "{v}");
        assert!(!v.contains(' ') && !v.ends_with('+'), "{v}");
    }
}
