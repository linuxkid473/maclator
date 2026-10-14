//! This binary is deliberately absent from helper trust. No write is requested.
fn main() {
    let request = macflow_protocol::Request {
        schema: macflow_protocol::SCHEMA,
        id: 1,
        action: macflow_protocol::Command::Status,
    };
    let requirement = macflow_platform::code_requirement_for_file(std::path::Path::new(
        "/Library/Application Support/MacFlow/Runtime/macflow-helper",
    ))
    .expect("installed signed helper");
    let result = macflow::ipc::call(
        macflow_protocol::HELPER_SERVICE,
        true,
        &request,
        Some(&requirement),
        std::time::Duration::from_secs(2),
    );
    match result {
        Err(error) => println!(
            "{}",
            serde_json::json!({"untrusted_client_rejected":true,"result":error})
        ),
        Ok(_) => {
            eprintln!("untrusted helper request unexpectedly received a response");
            std::process::exit(1);
        }
    }
}
