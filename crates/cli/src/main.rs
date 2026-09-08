use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(reccursive_cli::run_from_env())
}
