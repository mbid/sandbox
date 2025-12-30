use std::{env, process::ExitCode};

fn main() -> ExitCode {
    match sandbox::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            let has_backtrace = env::var("RUST_BACKTRACE").as_ref().map(|s| s.as_str()) == Ok("1");
            if has_backtrace {
                eprintln!("Backtrace:\n{}", e.backtrace());
            }

            ExitCode::FAILURE
        }
    }
}
