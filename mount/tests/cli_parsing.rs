use std::path::PathBuf;
use unidrive_mount::cli::{parse_args, CliError};

#[test]
fn both_args_present_parses_ok() {
    let argv = vec![
        "unidrive-mount".to_string(),
        "--mount".to_string(),
        "/tmp/mnt".to_string(),
        "--ipc".to_string(),
        "/tmp/jvm.sock".to_string(),
    ];
    let cli = parse_args(&argv).unwrap();
    assert_eq!(cli.mount, PathBuf::from("/tmp/mnt"));
    assert_eq!(cli.ipc, PathBuf::from("/tmp/jvm.sock"));
}

#[test]
fn missing_mount_returns_usage_error() {
    let argv = vec![
        "unidrive-mount".to_string(),
        "--ipc".to_string(),
        "/tmp/jvm.sock".to_string(),
    ];
    let err = parse_args(&argv).unwrap_err();
    assert!(matches!(err, CliError::Usage(_)), "expected CliError::Usage, got {err:?}");
}

#[test]
fn missing_ipc_returns_usage_error() {
    let argv = vec![
        "unidrive-mount".to_string(),
        "--mount".to_string(),
        "/tmp/mnt".to_string(),
    ];
    let err = parse_args(&argv).unwrap_err();
    assert!(matches!(err, CliError::Usage(_)));
}

#[test]
fn unknown_arg_returns_usage_error() {
    let argv = vec![
        "unidrive-mount".to_string(),
        "--mount".to_string(),
        "/tmp/mnt".to_string(),
        "--ipc".to_string(),
        "/tmp/jvm.sock".to_string(),
        "--bogus".to_string(),
    ];
    let err = parse_args(&argv).unwrap_err();
    assert!(matches!(err, CliError::Usage(_)));
}

#[test]
fn help_arg_returns_help() {
    let argv = vec!["unidrive-mount".to_string(), "--help".to_string()];
    let err = parse_args(&argv).unwrap_err();
    assert!(matches!(err, CliError::Help(_)));
}

#[test]
fn missing_value_after_mount_returns_usage_error() {
    let argv = vec!["unidrive-mount".to_string(), "--mount".to_string()];
    let err = parse_args(&argv).unwrap_err();
    assert!(matches!(err, CliError::Usage(_)));
}

fn argv(args: &[&str]) -> Vec<String> {
    std::iter::once("unidrive-mount").chain(args.iter().copied()).map(String::from).collect()
}

#[test]
fn token_file_and_profile_parse() {
    let cli = parse_args(&argv(&["--mount", "/m", "--ipc", "/s", "--ipc-token-file", "/c/p1/ipc.token", "--profile", "p1"])).unwrap();
    assert_eq!(cli.ipc_token_file, Some(PathBuf::from("/c/p1/ipc.token")));
    assert_eq!(cli.profile.as_deref(), Some("p1"));
}

#[test]
fn token_file_without_profile_is_usage_error() {
    let err = parse_args(&argv(&["--mount", "/m", "--ipc", "/s", "--ipc-token-file", "/t"])).unwrap_err();
    assert!(matches!(err, CliError::Usage(ref m) if m.contains("requires --profile")), "{err:?}");
}

#[test]
fn without_token_args_parses_as_before() {
    let cli = parse_args(&argv(&["--mount", "/m", "--ipc", "/s"])).unwrap();
    assert!(cli.ipc_token_file.is_none() && cli.profile.is_none());
}

#[test]
fn version_reports_ipc_protocol() {
    let err = parse_args(&argv(&["--version"])).unwrap_err();
    assert!(matches!(err, CliError::Version(ref m) if m.trim_end().ends_with("ipc-protocol 2")), "{err:?}");
}
