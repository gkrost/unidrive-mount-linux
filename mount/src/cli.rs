use std::path::PathBuf;

const USAGE: &str =
    "Usage: unidrive-mount --mount <path> --ipc <socket>\n\
     \n\
     Options:\n\
     \x20\x20--mount <path>   Filesystem mount point (an existing empty directory).\n\
     \x20\x20--ipc <socket>   Unix-domain-socket path to the unidrive JVM IpcServer.\n\
     \x20\x20--help           Show this message and exit.\n";

#[derive(Debug)]
pub struct Cli {
    pub mount: PathBuf,
    pub ipc: PathBuf,
}

#[derive(Debug)]
pub enum CliError {
    /// Argument-parsing error. The String is the message to print on stderr;
    /// `usage()` is appended by the caller. Exit 64 (EX_USAGE).
    Usage(String),
    /// `--help` requested. The String is the usage text to print on stdout. Exit 0.
    Help(String),
}

/// Parse a raw argv vector (argv[0] is the program name; values follow).
/// Returns `Ok(Cli)` on success, `Err(CliError::Usage(msg))` on bad args,
/// or `Err(CliError::Help(msg))` if `--help` was found.
pub fn parse_args(argv: &[String]) -> Result<Cli, CliError> {
    let mut mount: Option<PathBuf> = None;
    let mut ipc: Option<PathBuf> = None;
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
            other => {
                return Err(CliError::Usage(format!("unknown argument: {other}\n{USAGE}")));
            }
        }
        i += 1;
    }
    let mount = mount
        .ok_or_else(|| CliError::Usage(format!("missing required --mount\n{USAGE}")))?;
    let ipc = ipc.ok_or_else(|| CliError::Usage(format!("missing required --ipc\n{USAGE}")))?;
    Ok(Cli { mount, ipc })
}

pub fn usage() -> &'static str {
    USAGE
}
