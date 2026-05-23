use unidrive_mount::kernel_floor::{check_kernel_floor, KernelFloorError};

#[test]
fn check_kernel_floor_refuses_below_6_9() {
    let err = check_kernel_floor(Some("6.8.0-generic")).unwrap_err();
    assert!(matches!(err, KernelFloorError::TooOld { .. }));
    let msg = format!("{err}");
    assert!(msg.contains("6.9"), "stderr must cite the required kernel: {msg}");
    assert!(msg.contains("FUSE_PASSTHROUGH"), "stderr must cite the missing feature: {msg}");
}

#[test]
fn check_kernel_floor_accepts_6_9() {
    check_kernel_floor(Some("6.9.0-generic")).unwrap();
}

#[test]
fn check_kernel_floor_accepts_7_0() {
    check_kernel_floor(Some("7.0.9-070009-generic")).unwrap();
}

#[test]
fn check_kernel_floor_rejects_unparseable() {
    let err = check_kernel_floor(Some("not-a-version")).unwrap_err();
    assert!(matches!(err, KernelFloorError::Unparseable { .. }));
}
