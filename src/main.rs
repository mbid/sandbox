fn main() {
    if let Err(e) = sandbox::run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}
