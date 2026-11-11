//! Minimal client for `dsh --profile sdk` (newline-delimited JSON-RPC over stdio).
//!
//! This is the transport we can use to drop the `dsh web` HTTP/WebSocket layer.
//! It deliberately exposes only the documented SDK protocol, not dsh internals.

pub mod sessions;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum SdkEvent {
    Notification { method: String, params: Value },
    Request { id: Value, method: String, params: Value },
    Exited,
}

pub struct SdkClient {
    child: Child,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>,
    pub events: mpsc::UnboundedReceiver<SdkEvent>,
}

impl SdkClient {
    pub async fn launch(cwd: &str) -> Result<Self> {
        let (program, prefix) = if let Some(runtime) = dsh_host::DshRuntime::current() {
            (
                runtime.node.display().to_string(),
                vec![runtime.bin.display().to_string()],
            )
        } else if let Ok(bin) = std::env::var("DSH_BIN") {
            ("node".to_string(), vec![bin])
        } else {
            ("dsh".to_string(), Vec::new())
        };

        let mut cmd = Command::new(&program);
        cmd.args(&prefix)
            .arg("--profile")
            .arg("sdk")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = cmd.spawn().context("failed to spawn dsh sdk")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no sdk stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no sdk stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("no sdk stderr"))?;

        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[sdk] {line}");
            }
        });

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
                        let params = value.get("params").cloned().unwrap_or(Value::Null);
                        if let Some(id) = value.get("id").cloned() {
                            let _ = event_tx.send(SdkEvent::Request {
                                id,
                                method: method.to_string(),
                                params,
                            });
                        } else {
                            let _ = event_tx.send(SdkEvent::Notification {
                                method: method.to_string(),
                                params,
                            });
                        }
                        continue;
                    }
                    if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                        if let Some(tx) = pending.lock().unwrap().remove(&id) {
                            let result = match value.get("error") {
                                Some(error) => Err(error.clone()),
                                None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = tx.send(result);
                        }
                    }
                }
                let _ = event_tx.send(SdkEvent::Exited);
            });
        }

        Ok(Self {
            child,
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            next_id: AtomicU64::new(1),
            pending,
            events: event_rx,
        })
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(anyhow!("sdk {method} failed: {error}")),
            Err(_) => Err(anyhow!("sdk {method} channel closed")),
        }
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<()> {
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string();
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn initialize(
        &self,
        cwd: &str,
        provider: &str,
        model: &str,
        reasoning_effort: Option<&str>,
        approval_requests: bool,
        user_questions: bool,
    ) -> Result<Value> {
        let mut params = json!({
            "cwd": cwd,
            "provider": provider,
            "model": model,
            "approvalRequests": approval_requests,
        });
        if let Some(effort) = reasoning_effort {
            params["reasoningEffort"] = json!(effort);
        }
        if user_questions {
            params["capabilities"] = json!({ "userQuestions": true });
        }
        self.request("initialize", params).await
    }

    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<Value> {
        self.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "contentBlocks": [{ "type": "text", "text": text }],
            }),
        )
        .await
    }

    pub async fn resume(&self, session_id: &str) -> Result<()> {
        self.request("session/resume", json!({ "sessionId": session_id }))
            .await
            .map(|_| ())
    }

    pub async fn wait(&self, session_id: &str) -> Result<()> {
        self.request("session/wait", json!({ "sessionId": session_id }))
            .await
            .map(|_| ())
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.request("shutdown", Value::Null).await.map(|_| ())
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}
