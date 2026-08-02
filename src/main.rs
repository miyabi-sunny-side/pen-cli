fn main() {
    if let Err(error) = pen_cli::run(std::env::args().skip(1)) {
        eprintln!("pen: {error}");
        std::process::exit(1);
    }
}
