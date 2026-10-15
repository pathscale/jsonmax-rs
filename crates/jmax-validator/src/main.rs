//! Process adapter. Hosts supply live reactor bindings through the library API.
use std::{io, process::ExitCode};

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let code = jmax_validator::run(
        std::env::args_os().skip(1),
        &mut stdin.lock(),
        &mut stdout.lock(),
        &mut stderr.lock(),
    );
    ExitCode::from(code)
}
