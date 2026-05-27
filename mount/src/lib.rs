pub mod cache_scanner;
pub mod cli;
pub mod fuse_fs;
pub mod ipc;
pub mod kernel_floor;
pub mod logging;
pub mod path_map;
pub mod profile_lock;
pub mod reconnect;
pub mod run;

#[cfg(any(test, debug_assertions))]
pub mod fake_jvm;

use std::process::ExitCode;

pub fn run_main() -> ExitCode {
    run::run_main()
}
