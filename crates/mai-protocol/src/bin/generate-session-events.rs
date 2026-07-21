use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../mai-server/web/src/events/session-events.generated.ts");
    let expected = mai_protocol::session_events_typescript();
    if std::env::args().any(|argument| argument == "--check") {
        let actual = std::fs::read_to_string(&output)?;
        if actual != expected {
            return Err(format!(
                "{} 与 pl-protocol 声明不一致；请运行 generate-session-events",
                output.display()
            )
            .into());
        }
        return Ok(());
    }
    std::fs::write(&output, expected)?;
    println!("generated {}", output.display());
    Ok(())
}
