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
