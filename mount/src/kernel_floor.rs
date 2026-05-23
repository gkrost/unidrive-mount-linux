use std::fs;

#[derive(Debug, thiserror::Error)]
pub enum KernelFloorError {
    #[error("kernel {found} too old; FUSE_PASSTHROUGH requires Linux 6.9 or newer")]
    TooOld { found: String },
    #[error("could not parse kernel release {raw:?}; FUSE_PASSTHROUGH requires Linux 6.9 or newer")]
    Unparseable { raw: String },
    #[error("could not read /proc/sys/kernel/osrelease: {0}")]
    IoError(#[from] std::io::Error),
}

/// Returns Ok if running on Linux >= 6.9, else error explaining what's missing.
/// `osrelease_override` is for tests; production calls pass None and read from /proc.
pub fn check_kernel_floor(osrelease_override: Option<&str>) -> Result<(), KernelFloorError> {
    let raw = match osrelease_override {
        Some(s) => s.to_string(),
        None => fs::read_to_string("/proc/sys/kernel/osrelease")?,
    };
    let trimmed = raw.trim();
    let (major, minor) = parse_major_minor(trimmed)
        .ok_or_else(|| KernelFloorError::Unparseable { raw: trimmed.to_string() })?;
    if (major, minor) < (6, 9) {
        return Err(KernelFloorError::TooOld { found: trimmed.to_string() });
    }
    Ok(())
}

fn parse_major_minor(raw: &str) -> Option<(u32, u32)> {
    // Examples: "6.9.0-generic", "7.0.9-070009-generic", "5.15.0".
    let prefix = raw.split('-').next()?;
    let mut parts = prefix.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}
