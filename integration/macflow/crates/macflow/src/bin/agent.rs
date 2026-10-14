fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.iter().any(|a| a == "--helper-hold-smoke") {
        macflow::agent::helper_hold_smoke().map(|v| println!("{v}"))
    } else if args.iter().any(|a| a == "--helper-smoke") {
        macflow::agent::helper_smoke().map(|v| println!("{v}"))
    } else {
        macflow::agent::run()
    };
    if let Err(e) = result {
        eprintln!("MacFlow agent: {e}");
        std::process::exit(1)
    }
}
