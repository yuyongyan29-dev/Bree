//! Standalone main-thread controller, linked only to the library built with --cfg test.
//! This is not a Cargo integration test: libtest runs tests on worker threads.
use std::path::PathBuf;

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if ![3, 4].contains(&args.len()) {
        eprintln!(
            "usage: p2_native_harness [self-built fixture executable] [absolute run directory] [case]"
        );
        std::process::exit(64);
    }
    let executable = PathBuf::from(&args[1]);
    let run_root = PathBuf::from(&args[2]);
    match bree_cli::cleanup::p2_session_access::run(&executable, &run_root) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("p2_native_harness: {error}");
            std::process::exit(1);
        }
    }
}
