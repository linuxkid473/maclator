fn main() {
    if let Err(e) = macflow::helper::run() {
        eprintln!("MacFlow helper: {e}");
        std::process::exit(1)
    }
}
