//! Typert Remote 客户端：一元调用（POST /api/<endpoint>）与流（WS /api/remote.mux）。
//!
//! 线协议事实（来自 `@deepseek-ai/dsh-client-connection` 与 `dsh-api-gateway`）：
//! - unary: `POST /api/<endpoint>`，body `{type:"client-request", rpcId, method, payload}`
//! - 响应: `{rpcId, result: {ok:true, value} | {ok:false, error}}`
//! - stream: WS `/api/remote.mux`，帧 `open|item|end|cancel`（client→host）与 `item|end|error`（host→client）
//! - 全局事件: 逻辑端点 `$events`，payload `{args:{}}`

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone)]
pub enum Frame {
    Item(Value),
    End,
    Error(Value),
}

pub struct Rpc<'a> {
    client: &'a reqwest::Client,
    base: url::Url,
}

impl<'a> Rpc<'a> {
    pub fn new(client: &'a reqwest::Client, base: &url::Url) -> Self {
        Self {
            client,
            base: base.clone(),
        }
    }

    pub async fn call(&self, endpoint: &str, payload: Value) -> Result<Value> {
        let rpc_id = uuid::Uuid::new_v4().to_string();
        let url = self.base.join(&format!("api/{endpoint}"))?;
        let body = json!({
            "type": "client-request",
            "rpcId": rpc_id,
            "method": endpoint,
            "payload": { "args": payload },
        });
        let resp = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body)?)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("{endpoint}: HTTP {status}: {}", truncate(&text)));
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("{endpoint}: bad json ({e}): {}", truncate(&text)))?;
        if parsed.get("rpcId").and_then(|v| v.as_str()) != Some(rpc_id.as_str()) {
            return Err(anyhow!("{endpoint}: rpcId mismatch"));
        }
        let result = parsed
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{endpoint}: missing result: {}", truncate(&text)))?;
        if result.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            Ok(result.get("value").cloned().unwrap_or(Value::Null))
        } else {
            Err(anyhow!(
                "{endpoint}: {}",
                result
                    .get("error")
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "remote error".into())
            ))
        }
    }
}

/// One established connection generation (a socket whose `$events` delivered `ready`).
#[derive(Debug, Clone)]
pub struct Generation {
    pub number: u64,
    pub client_id: String,
    pub home: Option<String>,
}

enum OutMsg {
    Text(String),
    Ping,
    Close,
}

struct StreamEntry {
    endpoint: String,
    payload: Value,
    sender: mpsc::UnboundedSender<Frame>,
}

pub type CookieFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;
pub type CookieProvider = Arc<dyn Fn() -> CookieFuture + Send + Sync>;

struct Inner {
    ws_url: String,
    cookie_provider: CookieProvider,
    /// Present only while a WebSocket link is up.
    out: Mutex<Option<mpsc::UnboundedSender<OutMsg>>>,
    /// Logical streams stay registered across reconnects and are reopened on ready.
    streams: Mutex<HashMap<String, StreamEntry>>,
    events_tx: mpsc::UnboundedSender<Value>,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
    generation_tx: watch::Sender<Option<Generation>>,
    generation_seq: AtomicU64,
}

impl Inner {
    fn send_frame(&self, value: Value) {
        let guard = self.out.lock().unwrap();
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(OutMsg::Text(value.to_string()));
        }
    }
}

/// A reconnecting mux. `open()` can be called at any time: streams are queued
/// until a `$events` ready handshake, reopened after every reconnect (the host
/// then sends a fresh snapshot), and `$events` itself is owned here so callers
/// survive link loss without rewiring.
#[derive(Clone)]
pub struct Mux {
    inner: Arc<Inner>,
}

pub struct StreamHandle {
    pub stream_id: String,
    rx: mpsc::UnboundedReceiver<Frame>,
    inner: Arc<Inner>,
}

impl StreamHandle {
    pub async fn recv(&mut self) -> Option<Frame> {
        self.rx.recv().await
    }

    pub fn send_item(&self, value: Value) {
        self.inner.send_frame(json!({
            "type": "item",
            "streamId": self.stream_id,
            "value": value,
        }));
    }

    pub fn end(&self) {
        self.inner.send_frame(json!({
            "type": "end",
            "streamId": self.stream_id,
        }));
    }

    /// Cancels the stream and forgets it, so a reconnect will not reopen it.
    pub fn cancel(&self) {
        self.inner.streams.lock().unwrap().remove(&self.stream_id);
        self.inner.send_frame(json!({
            "type": "cancel",
            "streamId": self.stream_id,
        }));
    }
}

impl Mux {
    /// Starts the supervisor and waits for the first `$events` ready.
    /// [cookie] is used as-is for every reconnect.
    pub async fn connect(ws_url: &str, cookie: Option<String>) -> Result<Self> {
        let provider: CookieProvider = Arc::new(move || {
            let cookie = cookie.clone();
            Box::pin(async move { cookie.ok_or_else(|| anyhow!("no cookie")) })
        });
        Self::connect_with_provider(ws_url, provider).await
    }

    /// Starts with a provider that is called on every connect attempt, so a
    /// one-shot `/connect` operator cookie can be re-minted after link loss.
    pub async fn connect_with_provider(
        ws_url: &str,
        cookie_provider: CookieProvider,
    ) -> Result<Self> {
        let (events_tx, events_rx) = mpsc::unbounded_channel::<Value>();
        let (generation_tx, mut ready_rx) = watch::channel::<Option<Generation>>(None);
        let inner = Arc::new(Inner {
            ws_url: ws_url.to_string(),
            cookie_provider,
            out: Mutex::new(None),
            streams: Mutex::new(HashMap::new()),
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
            generation_tx,
            generation_seq: AtomicU64::new(0),
        });
        tokio::spawn(supervise(inner.clone()));

        let ready = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if ready_rx.borrow().is_some() {
                    return true;
                }
                if ready_rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await;
        match ready {
            Ok(true) => Ok(Self { inner }),
            Ok(false) => Err(anyhow!("mux supervisor stopped before ready")),
            Err(_) => Err(anyhow!("timed out waiting for $events ready")),
        }
    }

    /// Takes the single host-event receiver (`emit` / `waterfall` / `cancel` items).
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<Value>> {
        self.inner.events_rx.lock().unwrap().take()
    }

    /// Fires `Some(generation)` on every successful connect and `None` on loss.
    pub fn generations(&self) -> watch::Receiver<Option<Generation>> {
        self.inner.generation_tx.subscribe()
    }

    /// `clientId` of the current `$events` generation, for `$events/result`.
    /// Closes the current link; the supervisor reconnects and resyncs streams.
    pub fn force_reconnect(&self) {
        let guard = self.inner.out.lock().unwrap();
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(OutMsg::Close);
        }
    }

    pub fn client_id(&self) -> Option<String> {
        self.inner
            .generation_tx
            .borrow()
            .as_ref()
            .map(|g| g.client_id.clone())
    }

    pub async fn open(&self, endpoint: &str, payload: Value) -> Result<StreamHandle> {
        let stream_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.streams.lock().unwrap().insert(
            stream_id.clone(),
            StreamEntry {
                endpoint: endpoint.to_string(),
                payload: payload.clone(),
                sender: tx,
            },
        );
        // If the link is not ready this is dropped; the ready handler opens
        // every registered stream anyway.
        self.inner.send_frame(json!({
            "type": "open",
            "streamId": stream_id,
            "endpoint": endpoint,
            "payload": { "args": payload },
        }));
        Ok(StreamHandle {
            stream_id,
            rx,
            inner: self.inner.clone(),
        })
    }
}

async fn supervise(inner: Arc<Inner>) {
    let mut attempt = 0usize;
    loop {
        match connect_once(&inner, &mut attempt).await {
            Ok(()) => {}
            Err(err) => {
                eprintln!("[mux] connect failed: {err}");
                attempt = attempt.saturating_add(1);
            }
        }
        let _ = inner.generation_tx.send(None);
        *inner.out.lock().unwrap() = None;
        let delay = backoff(attempt);
        eprintln!("[mux] reconnecting in {} ms", delay.as_millis());
        tokio::time::sleep(delay).await;
    }
}

fn backoff(attempt: usize) -> Duration {
    const MS: [u64; 7] = [300, 600, 1200, 2500, 5000, 10000, 20000];
    Duration::from_millis(MS[attempt.min(MS.len() - 1)])
}

/// One WebSocket lifetime. Returns when the link dies; the caller reconnects.
async fn connect_once(inner: &Arc<Inner>, attempt: &mut usize) -> Result<()> {
    let cookie = (inner.cookie_provider)().await?;
    eprintln!("[mux] connecting cookie={} bytes", cookie.len());
    let mut request = inner.ws_url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("cookie", cookie.parse().map_err(|_| anyhow!("bad cookie header"))?);
    let (socket, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(socket) => socket,
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let status = response.status();
            let body = response
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).to_string())
                .unwrap_or_default();
            return Err(anyhow!("HTTP {status}: {}", body.chars().take(300).collect::<String>()));
        }
        Err(err) => return Err(err.into()),
    };
    let (mut sink, mut stream) = socket.split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<OutMsg>();
    *inner.out.lock().unwrap() = Some(out_tx.clone());

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let message = match msg {
                OutMsg::Text(text) => Message::Text(text.into()),
                OutMsg::Ping => Message::Ping(Vec::new().into()),
                OutMsg::Close => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            };
            if sink.send(message).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // The control stream must be opened before anything can be considered ready.
    let events_id = uuid::Uuid::new_v4().to_string();
    let _ = out_tx.send(OutMsg::Text(
        json!({
            "type": "open",
            "streamId": events_id,
            "endpoint": "$events",
            "payload": { "args": {} },
        })
        .to_string(),
    ));

    let mut last_rx = Instant::now();
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                last_rx = Instant::now();
                let text = match msg {
                    Message::Text(text) => text.to_string(),
                    Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => continue,
                    _ => continue,
                };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    eprintln!("[mux] bad json: {}", text.chars().take(120).collect::<String>());
                    continue;
                };
                let stream_id = frame.get("streamId").and_then(|v| v.as_str()).unwrap_or("");
                let kind = frame.get("type").and_then(|v| v.as_str()).unwrap_or("");

                if stream_id == events_id {
                    match kind {
                        "item" => {
                            let value = frame.get("value").cloned().unwrap_or(Value::Null);
                            if value.get("type").and_then(|v| v.as_str()) == Some("ready") {
                                let number = inner.generation_seq.fetch_add(1, Ordering::SeqCst) + 1;
                                let generation = Generation {
                                    number,
                                    client_id: value
                                        .get("clientId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    home: value
                                        .pointer("/host/home")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                };
                                *attempt = 0;
                                eprintln!(
                                    "[mux] ready generation {} client={}",
                                    generation.number, generation.client_id
                                );
                                let _ = inner.generation_tx.send(Some(generation));
                                // Resync: reopen every logical stream on this new link.
                                let opens: Vec<Value> = inner
                                    .streams
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .map(|(id, entry)| {
                                        json!({
                                            "type": "open",
                                            "streamId": id,
                                            "endpoint": entry.endpoint,
                                            "payload": { "args": entry.payload },
                                        })
                                    })
                                    .collect();
                                for open in opens {
                                    let _ = out_tx.send(OutMsg::Text(open.to_string()));
                                }
                            } else {
                                let _ = inner.events_tx.send(value);
                            }
                        }
                        "end" | "error" => break,
                        _ => {}
                    }
                    continue;
                }

                if stream_id.is_empty() {
                    continue;
                }
                let parsed = match kind {
                    "item" => Frame::Item(frame.get("value").cloned().unwrap_or(Value::Null)),
                    "end" => Frame::End,
                    "error" => Frame::Error(frame.get("error").cloned().unwrap_or(Value::Null)),
                    _ => continue,
                };
                let terminal = !matches!(parsed, Frame::Item(_));
                let sender = inner
                    .streams
                    .lock()
                    .unwrap()
                    .get(stream_id)
                    .map(|entry| entry.sender.clone());
                if let Some(sender) = sender {
                    let _ = sender.send(parsed);
                    if terminal {
                        inner.streams.lock().unwrap().remove(stream_id);
                    }
                }
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > Duration::from_secs(60) {
                    eprintln!("[mux] link silent for 60s, reconnecting");
                    break;
                }
                let _ = out_tx.send(OutMsg::Ping);
            }
        }
    }

    *inner.out.lock().unwrap() = None;
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

fn truncate(s: &str) -> String {
    s.chars().take(400).collect()
}
