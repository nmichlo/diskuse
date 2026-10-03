use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(diskuse::cli(std::env::args_os()))
}
