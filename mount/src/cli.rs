use std::path::PathBuf;

const USAGE: &str =
    "Usage: unidrive-mount --mount <path> --ipc <socket> [--cache <path>] [--lock <path>]\n\
     \x20\x20                     [--ipc-token-file <path> --profile <name>]\n\
     \x20\x20     unidrive-mount --version\n\
     \n\
     Options:\n\
     \x20\x20--mount <path>   Filesystem mount point (an existing empty directory).\n\
     \x20\x20--ipc <socket>   Unix-domain-socket path to the unidrive JVM IpcServer.\n\
     \x20\x20--cache <path>   LocalCache root for crash-recovery scan at startup.\n\
     \x20\x20                 Defaults to $XDG_CACHE_HOME/unidrive/hydration, or\n\
     \x20\x20                 $HOME/.cache/unidrive/hydration if XDG_CACHE_HOME unset.\n\
     \x20\x20--lock <path>    Per-profile lock file. When supplied, the co-daemon\n\
     \x20\x20                 acquires its own flock(2) on this path for the session,\n\
     \x20\x20                 closing the kill -9 race with the JVM-side ProcessLock.\n\
     \x20\x20--ipc-token-file <path>\n\
     \x20\x20                 The profile's IPC token file (ipc.token, scope full).\n\
     \x20\x20                 Read again on every connect; every connection runs\n\
     \x20\x20                 the IPC protocol-2 handshake with it. Requires --profile.\n\
     \x20\x20--profile <name> Profile name exactly as in config.toml (part of the\n\
     \x20\x20                 handshake proof).\n\
     \x20\x20--version        Print the version and the IPC protocol, then exit.\n\
     \x20\x20--help           Show this message and exit.\n";

#[derive(Debug)]
pub struct Cli {
    pub mount: PathBuf,
    pub ipc: PathBuf,
    pub cache: PathBuf,
    pub lock: Option<PathBuf>,
    pub ipc_token_file: Option<PathBuf>,
    pub profile: Option<String>,
}

#[derive(Debug)]
pub enum CliError {
    Usage(String),
    Help(String),
    Version(String),
}

/// Parse a raw argv vector (argv[0] is the program name; values follow).
/// Returns `Ok(Cli)` on success, `Err(CliError::Usage(msg))` on bad args,
/// or `Err(CliError::Help(msg))` if `--help` was found.
pub fn parse_args(argv: &[String]) -> Result<Cli, CliError> {
    let mut mount: Option<PathBuf> = None;
    let mut ipc: Option<PathBuf> = None;
    let mut cache: Option<PathBuf> = None;
    let mut lock: Option<PathBuf> = None;
    let mut ipc_token_file: Option<PathBuf> = None;
    let mut profile: Option<String> = None;
    let mut i = 1; // skip argv[0]
    while i < argv.len() {
        let arg = &argv[i];
        match arg.as_str() {
            "--help" | "-h" => {
                return Err(CliError::Help(USAGE.to_string()));
            }
            "--mount" => {
                i += 1;
                if i >= argv.len() {
                    return Err(CliError::Usage(format!("--mount requires a value\n{USAGE}")));
                }
                mount = Some(PathBuf::from(&argv[i]));
            }
            "--ipc" => {
                i += 1;
                if i >= argv.len() {
                    return Err(CliError::Usage(format!("--ipc requires a value\n{USAGE}")));
                }
                ipc = Some(PathBuf::from(&argv[i]));
            }
            "--cache" => {
                i += 1;
                if i >= argv.len() {
                    return Err(CliError::Usage(format!("--cache requires a value\n{USAGE}")));
                }
                cache = Some(PathBuf::from(&argv[i]));
            }
            "--lock" => {
                i += 1;
                if i >= argv.len() {
                    return Err(CliError::Usage(format!("--lock requires a value\n{USAGE}")));
                }
                lock = Some(PathBuf::from(&argv[i]));
            }
            "--version" => {
                return Err(CliError::Version(format!(
                    "unidrive-mount {} ipc-protocol {}\n",
                    env!("CARGO_PKG_VERSION"),
                    crate::ipc_auth::PROTOCOL
                )));
            }
            "--ipc-token-file" => {
                i += 1;
                if i >= argv.len() {
                    return Err(CliError::Usage(format!("--ipc-token-file requires a value\n{USAGE}")));
                }
                ipc_token_file = Some(PathBuf::from(&argv[i]));
            }
            "--profile" => {
                i += 1;
                if i >= argv.len() {
                    return Err(CliError::Usage(format!("--profile requires a value\n{USAGE}")));
                }
                profile = Some(argv[i].clone());
            }
            other => {
                return Err(CliError::Usage(format!("unknown argument: {other}\n{USAGE}")));
            }
        }
        i += 1;
    }
    let mount = mount
        .ok_or_else(|| CliError::Usage(format!("missing required --mount\n{USAGE}")))?;
    let ipc = ipc.ok_or_else(|| CliError::Usage(format!("missing required --ipc\n{USAGE}")))?;
    if ipc_token_file.is_some() && profile.is_none() {
        return Err(CliError::Usage(format!("--ipc-token-file requires --profile\n{USAGE}")));
    }
    let cache = cache.unwrap_or_else(default_cache_root);
    Ok(Cli { mount, ipc, cache, lock, ipc_token_file, profile })
}

fn default_cache_root() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("unidrive").join("hydration");
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h)
            .join(".cache")
            .join("unidrive")
            .join("hydration");
    }
    PathBuf::from(".cache/unidrive/hydration")
}

pub fn usage() -> &'static str {
    USAGE
}
