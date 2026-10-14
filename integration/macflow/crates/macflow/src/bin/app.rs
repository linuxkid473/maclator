fn main() {
    if std::env::args().any(|a| a == "--register-services") {
        if let Err(e) = macflow::lifecycle::register_or_fallback() {
            eprintln!("MacFlow services: {e}");
        }
    }
    if let Err(e) = macflow::ui::run() {
        eprintln!("MacFlow: {e}");
        std::process::exit(1)
    }
}
