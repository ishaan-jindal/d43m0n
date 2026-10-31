fn main() {
    if let Err(e) = d43m0n::daemon::run() {
        eprintln!("d43m0nd: {e:#}");
        std::process::exit(1);
    }
}
