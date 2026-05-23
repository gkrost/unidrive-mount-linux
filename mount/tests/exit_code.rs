use assert_cmd::Command;

#[test]
fn binary_links_and_help_exits_zero() {
    // Smoke test: the binary parses, links, and exits 0 on `--help`. The
    // kernel-floor logic is covered by the kernel_floor unit tests; the
    // arg-parsing logic by cli_parsing.rs. This test just confirms the
    // binary is buildable end-to-end on the host.
    let mut cmd = Command::cargo_bin("unidrive-mount").unwrap();
    cmd.arg("--help").assert().success();
}

#[test]
fn missing_args_exits_with_64() {
    // No args -> EX_USAGE from cli::parse_args.
    let mut cmd = Command::cargo_bin("unidrive-mount").unwrap();
    cmd.assert().failure().code(64);
}
