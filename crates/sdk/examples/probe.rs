use anyhow::Result;
use dsh_sdk::{SdkClient, SdkEvent};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let mut client = SdkClient::launch("/tmp").await?;
    println!("initialize: {:?}", client.initialize(
        "/tmp",
        "deepseek-official",
        "deepseek-v4-pro",
        Some("high"),
        true,
        true,
    ).await?);
    let session = &format!("sdk-probe-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis());
    println!("prompt: {:?}", client.prompt(session, "Reply with exactly: SDK_OK").await?);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let event = tokio::time::timeout_at(deadline, client.events.recv()).await;
        let Ok(Some(event)) = event else { break };
        match event {
            SdkEvent::Notification { method, params } => {
                if method == "session.event" {
                    let kind = params.pointer("/event/type").and_then(|v| v.as_str()).unwrap_or("?");
                    println!("event: {kind}");
                } else if method == "session.status" {
                    println!("status: {params}");
                    if params.get("status").and_then(|v| v.as_str()) == Some("idle") {
                        break;
                    }
                }
            }
            SdkEvent::Request { id, method, params } => {
                println!("request: {method} {params}");
                if method == "approval/request" {
                    client.respond(id, json!({ "outcome": "allowed-once" })).await?;
                } else if method == "user-question/ask" {
                    client.respond(id, json!({ "cancelled": true })).await?;
                }
            }
            SdkEvent::Exited => break,
        }
    }
    let _ = client.shutdown().await;
    Ok(())
}
