pub mod cli;
pub mod ipc;
pub mod kernel_floor;
pub mod path_map;

#[cfg(any(test, debug_assertions))]
pub mod fake_jvm;

use std::process::ExitCode;

pub fn run_main() -> ExitCode {
    // Real impl lands in Task 2/3; for Task 1 we only need kernel_floor wired.
    match kernel_floor::check_kernel_floor(None) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(78) // EX_CONFIG
        }
    }
}
