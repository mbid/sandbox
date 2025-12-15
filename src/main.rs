fn main() {
    if let Err(e) = sandbox::run() {
        eprintln!("Error: {:#}", e);
        if std::env::var("RUST_BACKTRACE").is_ok() {
            eprintln!("\nBacktrace:\n{}", e.backtrace());
        }
        std::process::exit(1);
    }
}
