fn main() {
    println!(
        "{}",
        serde_json::json!({"fresh_foreground_pid":macflow_platform::foreground_pid()})
    );
}
