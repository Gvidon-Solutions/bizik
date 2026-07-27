fn main() {
    if let Err(error) = bizik::run() {
        eprintln!("bzk: {error:#}");
        std::process::exit(1);
    }
}
