//! Process adapter for the library's stream-injected command driver.
use std::{io, process::ExitCode};

#[global_allocator]
static ALLOCATOR: rusty_alloc::RustyAlloc = rusty_alloc::RustyAlloc;

fn main() -> ExitCode {
    let stdout = io::stdout();
    let stderr = io::stderr();
    let code = jmax_gen_cli::run(
        std::env::args_os().skip(1),
        &mut stdout.lock(),
        &mut stderr.lock(),
    );
    ExitCode::from(code)
}
