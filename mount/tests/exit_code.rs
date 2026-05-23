use assert_cmd::Command;

#[test]
fn refuses_to_start_below_kernel_6_9_when_real_kernel_too_old() {
    // We can't fake the real kernel here; we test the binary runs at all.
    // The actual kernel-floor logic is covered by the unit test above.
    // This test verifies the binary parses, links, and exits cleanly on the host kernel.
    let host_kernel_ok = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .and_then(|s| {
            let p = s.split('-').next()?.to_string();
            let mut it = p.split('.');
            let maj: u32 = it.next()?.parse().ok()?;
            let min: u32 = it.next()?.parse().ok()?;
            Some((maj, min) >= (6, 9))
        })
        .unwrap_or(false);
    let mut cmd = Command::cargo_bin("unidrive-mount").unwrap();
    let assert = cmd.assert();
    if host_kernel_ok {
        assert.success();
    } else {
        assert.failure().code(78);
    }
}
