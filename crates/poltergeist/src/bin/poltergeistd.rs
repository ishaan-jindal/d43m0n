fn main() {
    if let Err(e) = poltergeist::daemon::run() {
        eprintln!("poltergeistd: {e:#}");
        std::process::exit(1);
    }
}
