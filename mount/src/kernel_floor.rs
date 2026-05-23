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
pub fn check_kernel_floor(_osrelease_override: Option<&str>) -> Result<(), KernelFloorError> {
    // Stub; real impl lands in Step 1.2.
    Ok(())
}
