//! OMD：dsh 的原生 GUI 客户端。
//!
//! 结构：GPUI（UI 线程）↔ async_channel ↔ worker 线程（tokio：host + RPC + 流）。
//! 协议细节见 crates/host 与 crates/transport。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_channel::{Receiver, Sender};
use dsh_host::{DshHost, DshRuntime};
use term::{Span, Term};
use dsh_transport::{CookieProvider, Frame, Mux, Rpc};
mod ghostty_vt;
mod term;

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

use gpui_kit::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::text::TextView;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 通信类型
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct SessionRow {
    id: String,
    cwd: String,
    title: String,
    running: bool,
    blank: bool,
    updated_at: i64,
}

/// Tern/Herdr-style workspace: a cwd groups conversations (tabs).
#[derive(Clone, Debug)]
struct Space {
    id: String,
    name: String,
    cwd: String,
    tabs: Vec<String>,
    active: Option<String>,
}


enum TerminalUp {
    Write(String),
    Resize(u16, u16),
}

async fn rows_first_session(rpc: &Rpc<'_>) -> Option<String> {
    let list = rpc
        .call("session/list", json!({ "_request": {} }))
        .await
        .ok()?;
    list.get("items")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|i| i.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

enum Cmd {
    Refresh,
    ModelCatalog,
    SelectModel { provider: String, model: String },
    Follow(String),
    NewSession { cwd: Option<String> },
    NewTerminal,
    Prompt { text: String, request_id: String },
    TerminalInput(String),
    Cancel,
    Decide { event_id: String, allow: bool },
    AnswerQuestion { event_id: String, answer: Value },
    UpdateDsh,
}

enum Ev {
    HostReady(String),
    Sessions(Vec<SessionRow>),
    ModelCatalog(Value),
    ModelSelected { provider: String, model: String },
    FollowStart(String),
    Event(Value),
    Line(String),
    StreamStart,
    StreamDelta(String),
    ReasoningDelta(String),
    StreamEnd,
    Connection(bool),
    AgentRunning(bool),
    ImageB64(String),
    TerminalOutput(String),
    TerminalScreen(Vec<String>),
    Approval { event_id: String, label: String },
    ApprovalGone,
    Question { event_id: String, request: Value },
    QuestionGone,
    DshInfo { version: String, bundled: bool },
    DshUpdate { done: bool, message: String },
    Error(String),
}

// ---------------------------------------------------------------------------
// worker：持有 host / rpc / mux，串行处理命令与流帧
// ---------------------------------------------------------------------------

fn spawn_worker(cmds: Receiver<Cmd>, evs: Sender<Ev>) {
    eprintln!("[worker] thread starting");
    std::thread::spawn(move || {
        eprintln!("[worker] thread running");
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                let _ = evs.send_blocking(Ev::Error(format!("tokio: {err}")));
                return;
            }
        };
        rt.block_on(worker_main(cmds, evs));
    });
}

async fn worker_main(cmds: Receiver<Cmd>, evs: Sender<Ev>) {
    let worker_started = std::time::Instant::now();
    // First run on a clean machine: install the engine into the app directory,
    // mirroring Tern's `share/tern/omp` layout. A system `dsh` is only a
    // development fallback.
    if DshRuntime::current().is_none()
        && std::env::var("DSH_BIN").is_err()
        && std::env::var("DSH_GUI_NO_AUTO_INSTALL").is_err()
    {
        let _ = evs
            .send(Ev::DshUpdate {
                done: false,
                message: "正在安装内置 dsh 引擎…".into(),
            })
            .await;
        match dsh_host::install_dsh("latest").await {
            Ok(runtime) => {
                let _ = evs
                    .send(Ev::DshUpdate {
                        done: true,
                        message: format!("内置引擎 v{} 已安装", runtime.version),
                    })
                    .await;
            }
            Err(err) => {
                let _ = evs
                    .send(Ev::DshUpdate {
                        done: true,
                        message: format!("内置引擎安装失败：{err}"),
                    })
                    .await;
            }
        }
    }
    if let Some(rows) = load_sessions_cache() {
        eprintln!("[worker] cached sessions: {}", rows.len());
        let _ = evs.send(Ev::Sessions(rows)).await;
    }
    eprintln!("[worker] launching host");
    let host = match DshHost::launch("web").await {
        Ok(host) => host,
        Err(err) => {
            let _ = evs.send(Ev::Error(format!("host launch failed: {err}"))).await;
            return;
        }
    };
    eprintln!(
        "[worker] host ready: {} ({}ms)",
        host.base(),
        worker_started.elapsed().as_millis()
    );
    let _ = evs.send(Ev::HostReady(host.base().to_string())).await;

    let rpc = Rpc::new(host.client(), host.base());
    let auth_client = host.client().clone();
    let auth_jar = host.jar();
    let auth_url = host.tokenized_url().clone();
    let cookie_provider: CookieProvider = Arc::new(move || {
        let client = auth_client.clone();
        let jar = auth_jar.clone();
        let url = auth_url.clone();
        Box::pin(async move { DshHost::auth_cookie(&client, &jar, &url).await })
    });
    let mux = match Mux::connect_with_provider(
        &host.ws_url("api/remote.mux").unwrap_or_default(),
        cookie_provider,
    )
    .await
    {
        Ok(mux) => mux,
        Err(err) => {
            let _ = evs.send(Ev::Error(format!("ws connect failed: {err}"))).await;
            return;
        }
    };

    // 连接状态：generation 出现即 connected，消失即 reconnecting。
    {
        let mut generations = mux.generations();
        let evs_status = evs.clone();
        tokio::spawn(async move {
            loop {
                let up = { generations.borrow().is_some() };
                let _ = evs_status.send(Ev::Connection(up)).await;
                if generations.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    // 测试钩子：N 秒后主动断开 WebSocket，验证重连 + snapshot resync。
    if let Ok(secs) = std::env::var("DSH_GUI_RECONNECT_AFTER") {
        if let Ok(secs) = secs.parse::<u64>() {
            let mux = mux.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                eprintln!("[worker] forcing reconnect");
                mux.force_reconnect();
            });
        }
    }

    // 流帧转发：Follow 时起一个 generation，旧帧直接丢弃。
    let (frame_tx, frame_rx) = async_channel::unbounded::<(u64, Value)>();
    let generation = Arc::new(AtomicU64::new(0));
    let mut current: Option<String> = None;
    let mut prompt_started: Option<std::time::Instant> = None;
    // 每次 Follow/NewSession 提升世代；旧 follow task 看到变化后主动取消流。
    let (follow_gen_tx, follow_gen_rx) = tokio::sync::watch::channel(0u64);
    // 本地回显过的 prompt rpcId；follow 回放同一条 user/message 时用它去重。
    let mut pending_echoes: VecDeque<String> = VecDeque::new();

    // 终端：attach 流 + uplink
    let terminal_target: tokio::sync::Mutex<Option<(String, String)>> =
        tokio::sync::Mutex::new(None);
    let (terminal_out, mut terminal_out_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let terminal_up_rx_slot: tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<TerminalUp>>> =
        tokio::sync::Mutex::new(None);
    let terminal_up_global: tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<TerminalUp>>> =
        tokio::sync::Mutex::new(None);

    // 常驻订阅全局事件（审批/提问都走这里）
    {
        let evs = evs.clone();
        let mux_events = mux.clone();
        let http = host.client().clone();
        let base = host.base().clone();
        tokio::spawn(async move {
            let result_rpc = Rpc::new(&http, &base);
            let Some(mut events) = mux_events.take_events() else {
                return;
            };
            while let Some(value) = events.recv().await {
                match value.get("type").and_then(|v| v.as_str()) {
                    Some("emit") => {
                        let event = value.get("event").and_then(|v| v.as_str()).unwrap_or("");
                        eprintln!("[worker] emit: {event}");
                        if event == "approval/request" {
                            eprintln!("[worker] approval event: {}", value.to_string().chars().take(600).collect::<String>());
                            let event_id = value
                                .get("eventId")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let label = value
                                .get("args")
                                .map(|a| a.to_string().chars().take(220).collect::<String>())
                                .unwrap_or_default();
                            let _ = evs.send(Ev::Approval { event_id, label }).await;
                        }
                    }
                    Some("waterfall") => {
                        let event = value.get("event").and_then(|v| v.as_str()).unwrap_or("");
                        let event_id = value
                            .get("eventId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if event == "approval/request" {
                            let req = value.get("request").cloned().unwrap_or(Value::Null);
                            let tool = req
                                .get("toolName")
                                .and_then(|v| v.as_str())
                                .unwrap_or("tool");
                            let reason = req
                                .get("reason")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let label = format!("{tool}：{reason}");
                            eprintln!("[worker] approval waterfall {event_id}");
                            let _ = evs
                                .send(Ev::Approval {
                                    event_id: event_id.clone(),
                                    label,
                                })
                                .await;

                            // 调试钩子：自动放行/拒绝，用于端到端验证
                            if let Ok(decision) = std::env::var("DSH_GUI_AUTODECIDE") {
                                let value = if decision == "allow" {
                                    "allowed-once"
                                } else {
                                    "rejected"
                                };
                                let client_id = mux_events.client_id().unwrap_or_default();
                                let resp = result_rpc
                                    .call("$events/result", json!({
                                        "clientId": client_id,
                                        "eventId": event_id,
                                        "outcome": { "kind": "result", "value": value },
                                    }))
                                    .await;
                                eprintln!("[worker] auto decide -> {value}: {resp:?}");
                                let _ = evs.send(Ev::ApprovalGone).await;
                            }
                        } else if event == "user-questions/request" {
                            let req = value.get("request").cloned().unwrap_or(Value::Null);
                            eprintln!("[worker] user questions {event_id}");
                            let _ = evs.send(Ev::Question { event_id, request: req }).await;
                        } else {
                            eprintln!("[worker] waterfall {event}");
                        }
                    }
                    other => {
                        eprintln!(
                            "[worker] events frame type={:?} raw={}",
                            other,
                            value.to_string().chars().take(200).collect::<String>()
                        );
                    }
                }
            }
        });
    }

    let _ = evs
        .send(Ev::DshInfo {
            version: host.version().to_string(),
            bundled: host.bundled(),
        })
        .await;
    let http = host.client().clone();
    let base = host.base().clone();
    let _ = evs.send(Ev::Line("本地引擎已就绪".into())).await;
    let _ = refresh_sessions(&rpc, &evs).await;

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Ok(cmd) = cmd else { break };
                match cmd {
                    Cmd::ModelCatalog => {
                        match rpc.call("session/modelCatalog", json!({})).await {
                            Ok(value) => {
                                let _ = evs.send(Ev::ModelCatalog(value)).await;
                            }
                            Err(err) => {
                                eprintln!("[worker] modelCatalog failed: {err}");
                            }
                        }
                    }
                    Cmd::SelectModel { provider, model } => {
                        if let Some(session_id) = current.clone() {
                            match rpc
                                .call("session/selectModel", json!({
                                    "request": {
                                        "sessionId": session_id,
                                        "provider": provider,
                                        "model": model,
                                    }
                                }))
                                .await
                            {
                                Ok(_) => {
                                    eprintln!("[worker] selectModel ok: {provider} / {model}");
                                    let _ = evs.send(Ev::ModelSelected {
                                        provider: provider.clone(),
                                        model: model.clone(),
                                    }).await;
                                }
                                Err(err) => {
                                    eprintln!("[worker] selectModel failed: {err}");
                                }
                            }
                        }
                    }
                    Cmd::Refresh => {
                        let _ = refresh_sessions(&rpc, &evs).await;
                    }
                    Cmd::Follow(id) => {
                        let gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
                        let _ = follow_gen_tx.send(gen);
                        current = Some(id.clone());
                        let _ = evs.send(Ev::FollowStart(id.clone())).await;
                        if let Ok(value) = rpc
                            .call("session/projections", json!({
                                "request": { "sessionId": id }
                            }))
                            .await
                        {
                            let selection = value
                                .pointer("/values/modelSelection/next")
                                .or_else(|| value.pointer("/values/modelSelection/lastUsed"));
                            if let Some(selection) = selection {
                                let provider = selection
                                    .get("provider")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let model = selection
                                    .get("model")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !model.is_empty() {
                                    let _ = evs.send(Ev::ModelSelected { provider, model }).await;
                                }
                            }
                        }

                        // 完全独立的 task，不在 select 的生命周期里
                        let mux_f = mux.clone();
                        let frame_tx_f = frame_tx.clone();
                        let evs_f = evs.clone();
                        let sid = id.clone();
                        let mut stop_rx = follow_gen_rx.clone();
                        // 本次世代已看过，避免 changed() 立即触发取消。
                        let _ = stop_rx.borrow_and_update();
                        let http_f = http.clone();
                        let base_f = base.clone();
                        tokio::task::spawn(async move {
                            trace(format_args!("[worker] follow task start {sid}"));
                            let mut stream = match mux_f
                                .open("session/follow", json!({
                                    "request": {
                                        "address": { "kind": "session", "sessionId": sid },
                                        "maxMessages": 200,
                                    }
                                }))
                                .await
                            {
                                Ok(s) => s,
                                Err(err) => {
                                    let _ = evs_f.send(Ev::Error(format!("follow failed: {err}"))).await;
                                    return;
                                }
                            };
                            trace(format_args!("[worker] follow stream opened id={}", stream.stream_id));
                            let mut n = 0u64;
                            let mut prompted = false;
                            loop {
                                tokio::select! {
                                    changed = stop_rx.changed() => {
                                        if changed.is_err() { break; }
                                        stream.cancel();
                                        break;
                                    }
                                    frame = stream.recv() => {
                                        match frame {
                                            Some(Frame::Item(v)) => {
                                                n += 1;
                                                if n == 1 && !prompted && std::env::var("DSH_GUI_SELFTEST").is_ok() {
                                                    prompted = true;
                                                    let rpc_f = Rpc::new(&http_f, &base_f);
                                                    let r = rpc_f.call("session/prompt", json!({
                                                        "request": {
                                                            "requestId": format!("self-{sid}"),
                                                            "sessionId": sid.clone(),
                                                            "mode": "queue",
                                                            "content": [ { "type": "text", "text": "Say: selftest2" } ],
                                                        }
                                                    })).await;
                                                    eprintln!("[worker] selftest prompt: {r:?}");
                                                }

                                                if frame_tx_f.send((gen, v)).await.is_err() { break; }
                                            }
                                            _ => break,
                                        }
                                    }
                                }
                            }
                            trace(format_args!("[worker] follow task EXITED after {n}"));
                        });
                        // 跳过旧的 spawn 块
                        continue;

                        // 调试钩子：DSH_GUI_AUTOPROMPT 时，follow 建立后自动发一条
                        if let Ok(text) = std::env::var("DSH_GUI_AUTOPROMPT") {
                            eprintln!("[worker] autoprompt after follow: {text}");
                            let result = rpc
                                .call("session/prompt", json!({
                                    "request": {
                                        "requestId": format!("omd-auto-{gen}"),
                                        "sessionId": id.clone(),
                                        "mode": "queue",
                                        "content": [ { "type": "text", "text": text } ],
                                    }
                                }))
                                .await;
                            eprintln!("[worker] autoprompt result: {result:?}");
                        }

                        // 独立 task：不在 select 里，避免被 select 的其他分支饿死

                    }
                    Cmd::NewSession { cwd: requested_cwd } => {
                        let cwd = requested_cwd
                            .or_else(|| std::env::var("PWD").ok())
                            .unwrap_or_else(|| "/".into());
                        match rpc
                            .call("session/create", json!({
                                "request": { "cwd": cwd }
                            }))
                            .await
                        {
                            Ok(created) => {
                                if let Some(session_id) =
                                    created.get("sessionId").and_then(|v| v.as_str())
                                {
                                    eprintln!("[worker] created session {session_id}");
                                    let _ = evs.send(Ev::Line(format!("+++ new session {session_id}"))).await;
                                    let gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
                                    let _ = follow_gen_tx.send(gen);
                                    current = Some(session_id.to_string());
                                    if let Ok(mut stream) = mux
                                        .open("session/follow", json!({
                                            "request": {
                                                "address": { "kind": "session", "sessionId": session_id },
                                            }
                                        }))
                                        .await
                                    {
                                        let frame_tx = frame_tx.clone();
                                        let mut stop_rx = follow_gen_rx.clone();
                                        // 本次世代已看过，避免 changed() 立即触发取消。
                                        let _ = stop_rx.borrow_and_update();
                                        tokio::spawn(async move {
                                            loop {
                                                tokio::select! {
                                                    changed = stop_rx.changed() => {
                                                        if changed.is_err() { break; }
                                                        stream.cancel();
                                                        break;
                                                    }
                                                    frame = stream.recv() => {
                                                        match frame {
                                                            Some(Frame::Item(value)) => {
                                                                if frame_tx.send((gen, value)).await.is_err() { break; }
                                                            }
                                                            _ => break,
                                                        }
                                                    }
                                                }
                                            }
                                        });
                                        if let Ok(b64) = std::env::var("DSH_GUI_AUTOIMAGE") {
                                            let result = rpc
                                                .call("session/prompt", json!({
                                                    "request": {
                                                        "requestId": format!("img-{gen}"),
                                                        "sessionId": session_id,
                                                        "mode": "queue",
                                                        "content": [
                                                            { "type": "image", "mediaType": "image/png", "data": b64 },
                                                            { "type": "text", "text": "Describe this image in 3 words" }
                                                        ],
                                                    }
                                                }))
                                                .await;
                                            eprintln!("[worker] autoimage prompt: {result:?}");
                                        }
                                        if let Ok(text) = std::env::var("DSH_GUI_AUTOPROMPT") {
                                            let result = rpc
                                                .call("session/prompt", json!({
                                                    "request": {
                                                        "requestId": format!("omd-new-{gen}"),
                                                        "sessionId": session_id,
                                                        "mode": "queue",
                                                        "content": [ { "type": "text", "text": text } ],
                                                    }
                                                }))
                                                .await;
                                            eprintln!("[worker] new-session prompt: {result:?}");
                                        }
                                    }
                                } else {
                                    let _ = evs.send(Ev::Error(format!("create returned {created}"))).await;
                                }
                            }
                            Err(err) => {
                                let _ = evs.send(Ev::Error(format!("session/create failed: {err}"))).await;
                            }
                        }
                        let _ = refresh_sessions(&rpc, &evs).await;
                    }
                    Cmd::NewTerminal => {
                        eprintln!("[worker] NewTerminal cmd received");
                        // 终端挂到新会话上，避免与浏览器/GUI 已占用的会话撞写权
                        let session_id = match rpc
                            .call("session/create", json!({ "request": { "cwd": "/tmp" } }))
                            .await
                        {
                            Ok(created) => created
                                .get("sessionId")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            Err(err) => {
                                let _ = evs.send(Ev::Error(format!("term session create failed: {err}"))).await;
                                None
                            }
                        };
                        eprintln!("[worker] NewTerminal session resolved: {session_id:?}");
                        let Some(session_id) = session_id else {
                            let _ = evs.send(Ev::Error("need a session for its terminal".into())).await;
                            continue;
                        };
                        let term_id = format!("omd-term-{}", uuid_like());
                        eprintln!("[worker] calling terminal/create…");
                        match rpc
                            .call("terminal/create", json!({
                                "agentId": session_id,
                                "request": { "id": term_id.clone(), "cols": 100, "rows": 30 }
                            }))
                            .await
                        {
                            Ok(_) => {
                                eprintln!("[worker] terminal created {term_id}");
                                *terminal_target.lock().await = Some((term_id.clone(), session_id.clone()));

                                // 每个终端独立 uplink channel；全局 sender 换成广播语义太重，
                                // v1 只支持一个活跃终端：直接用全局 terminal_up 的接收端
                                let mut up_rx = terminal_up_rx_slot.lock().await.take();
                                if up_rx.is_none() {
                                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<TerminalUp>();
                                    *terminal_up_global.lock().await = Some(tx);
                                    up_rx = Some(rx);
                                }
                                let Some(mut up_rx) = up_rx else { continue };

                                let mux = mux.clone();
                                let tx_out = terminal_out.clone();
                                let attach_id = term_id.clone();
                                let agent_for_attach = session_id.clone();
                                tokio::spawn(async move {
                                    let Ok(mut stream) = mux
                                        .open("terminal/attach", json!({ "agentId": agent_for_attach, "id": attach_id }))
                                        .await
                                    else { return };
                                    if std::env::var("DSH_GUI_AUTOTERM").is_ok() {
                                        stream.send_item(json!({
                                            "type": "write",
                                            "sequence": 1,
                                            "data": "ls --color=always / | head -5; echo COLORS-DONE\r",
                                        }));
                                    }
                                    let mut seq: u64 = 0;
                                    loop {
                                        tokio::select! {
                                            item = up_rx.recv() => {
                                                let Some(item) = item else { break };
                                                seq += 1;
                                                match item {
                                                    TerminalUp::Write(data) => stream.send_item(json!({
                                                        "type": "write", "sequence": seq, "data": data,
                                                    })),
                                                    TerminalUp::Resize(cols, rows) => stream.send_item(json!({
                                                        "type": "resize", "sequence": seq, "cols": cols, "rows": rows,
                                                    })),
                                                }
                                            }
                                            frame = stream.recv() => {
                                                match frame {
                                                    Some(Frame::Item(v)) => { let _ = tx_out.send(v); }
                                                    _ => break,
                                                }
                                            }
                                        }
                                    }
                                });
                            }
                            Err(err) => {
                                let _ = evs.send(Ev::Error(format!("terminal/create failed: {err}"))).await;
                            }
                        }
                    }
                    Cmd::TerminalInput(text) => {
                        if let Some(tx) = terminal_up_global.lock().await.as_ref() {
                            let _ = tx.send(TerminalUp::Write(text));
                        }
                    }
                    Cmd::Cancel => {
                        if let Some(session_id) = current.clone() {
                            match rpc
                                .call("session/cancel", json!({
                                    "request": { "sessionId": session_id }
                                }))
                                .await
                            {
                                Ok(_) => {
                                    eprintln!("[worker] cancel accepted");
                                    let _ = evs.send(Ev::Line("(已取消)".into())).await;
                                }
                                Err(err) => {
                                    let _ = evs.send(Ev::Error(format!("cancel failed: {err}"))).await;
                                }
                            }
                        }
                    }
                    Cmd::UpdateDsh => {
                        let evs = evs.clone();
                        tokio::spawn(async move {
                            let _ = evs
                                .send(Ev::DshUpdate {
                                    done: false,
                                    message: "正在下载并安装 dsh…".into(),
                                })
                                .await;
                            match dsh_host::install_dsh("latest").await {
                                Ok(runtime) => {
                                    let _ = evs
                                        .send(Ev::DshUpdate {
                                            done: true,
                                            message: format!(
                                                "已更新到 v{}，关闭并重新打开 App 后生效。",
                                                runtime.version
                                            ),
                                        })
                                        .await;
                                }
                                Err(err) => {
                                    let _ = evs
                                        .send(Ev::DshUpdate {
                                            done: true,
                                            message: format!("更新失败：{err}"),
                                        })
                                        .await;
                                }
                            }
                        });
                    }
                    Cmd::AnswerQuestion { event_id, answer } => {
                        let client_id = mux.client_id().unwrap_or_default();
                        let payload = json!({
                            "clientId": client_id,
                            "eventId": event_id,
                            "outcome": { "kind": "result", "value": answer },
                        });
                        match rpc.call("$events/result", payload).await {
                            Ok(_) => {
                                eprintln!("[worker] question answered");
                                let _ = evs.send(Ev::QuestionGone).await;
                            }
                            Err(err) => {
                                let _ = evs.send(Ev::Error(format!("question answer failed: {err}"))).await;
                            }
                        }
                    }
                    Cmd::Decide { event_id, allow } => {
                        let outcome = if allow { "allowed-once" } else { "rejected" };
                        let client_id = mux.client_id().unwrap_or_default();
                        let payload = json!({
                            "clientId": client_id,
                            "eventId": event_id,
                            "outcome": { "kind": "result", "value": outcome },
                        });
                        match rpc.call("$events/result", payload).await {
                            Ok(_) => {
                                eprintln!("[worker] decision sent: {outcome}");
                                let _ = evs.send(Ev::ApprovalGone).await;
                            }
                            Err(err) => {
                                let _ = evs.send(Ev::Error(format!("decide failed: {err}"))).await;
                            }
                        }
                    }
                    Cmd::Prompt { text, request_id } => {
                        eprintln!("[worker] Prompt cmd, current={current:?}");
                        let Some(session_id) = current.clone() else {
                            let _ = evs.send(Ev::Error("no session selected".into())).await;
                            continue;
                        };
                        // follow 回放这条消息时带同一个 source.rpcId，用它压掉本地回显。
                        pending_echoes.push_back(request_id.clone());
                        if pending_echoes.len() > 16 {
                            pending_echoes.pop_front();
                        }
                        prompt_started = Some(std::time::Instant::now());
                        let result = rpc
                            .call("session/prompt", json!({
                                "request": {
                                    "requestId": request_id,
                                    "sessionId": session_id,
                                    "mode": "queue",
                                    "content": [ { "type": "text", "text": text } ],
                                }
                            }))
                            .await;
                        match result {
                            Ok(value) => {
                                trace(format_args!("[worker] prompt accepted: {value}"));
                            }
                            Err(err) => {
                                prompt_started = None;
                                let _ = evs.send(Ev::Error(format!("prompt failed: {err}"))).await;
                            }
                        }
                    }
                }
            }
            term = terminal_out_rx.recv() => {
                if let Some(value) = term {
                    eprintln!("[worker] term frame: {}", value.to_string().chars().take(160).collect::<String>());
                    if let Some(data) = value.get("data").and_then(|d| d.as_str()) {
                        if value.get("type").and_then(|t| t.as_str()) == Some("output") {
                            let _ = evs.send(Ev::TerminalOutput(data.to_string())).await;
                        }
                    }
                }
            }
            frame = frame_rx.recv() => {
                if let Ok((gen, value)) = frame {
                    if gen == generation.load(Ordering::SeqCst) {
                        if let Some(kind) = value.get("type").and_then(|v| v.as_str()) {
                            if kind == "assistant-stream" {
                                let frame = value.get("frame").cloned().unwrap_or(Value::Null);
                                match frame.get("type").and_then(|v| v.as_str()) {
                                    Some("start") => {
                                        let _ = evs.send(Ev::StreamStart).await;
                                    }
                                    Some("chunk") => {
                                        let chunk = frame.get("chunk").cloned().unwrap_or(Value::Null);
                                        let chunk_type =
                                            chunk.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                        if matches!(chunk_type, "text-delta" | "reasoning-delta") {
                                            if let Some(started) = prompt_started.take() {
                                                eprintln!(
                                                    "[perf] first token: {}ms",
                                                    started.elapsed().as_millis()
                                                );
                                            }
                                        }
                                        if chunk_type == "text-delta" {
                                            if let Some(text) =
                                                chunk.get("text").and_then(|v| v.as_str())
                                            {
                                                let _ = evs.send(Ev::StreamDelta(text.to_string())).await;
                                            }
                                        } else if chunk_type == "reasoning-delta" {
                                            if let Some(text) =
                                                chunk.get("text").and_then(|v| v.as_str())
                                            {
                                                let _ = evs.send(Ev::ReasoningDelta(text.to_string())).await;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }
                        }
                        let event_type = value
                            .get("event")
                            .and_then(|e| e.get("type"))
                            .and_then(|v| v.as_str());
                        // 用户消息已本地回显：同 rpcId 的 follow 事件不再重复上屏。
                        let mut skip_echo = false;
                        if event_type == Some("user/message") {
                            if let Some(rpc_id) = value
                                .pointer("/event/data/source/rpcId")
                                .and_then(|v| v.as_str())
                            {
                                if let Some(pos) = pending_echoes.iter().position(|id| id == rpc_id) {
                                    pending_echoes.remove(pos);
                                    skip_echo = true;
                                }
                            }
                        }
                        // 图片消息：拉 attachment base64
                        if matches!(event_type, Some("user/message") | Some("assistant/message") | Some("tool/result")) {
                            if let Some(content) = value.pointer("/event/data/content").and_then(|c| c.as_array()) {
                                let session_id_img = current.clone().unwrap_or_default();
                                for block in content {
                                    if block.get("type").and_then(|t| t.as_str()) == Some("image") {
                                        let Some(att_id) = block.pointer("/attachment/attachmentId").and_then(|v| v.as_str()) else { continue };
                                        if att_id.is_empty() || session_id_img.is_empty() { continue; }
                                        let att_id = att_id.to_string();
                                        let rpc_img = http.clone();
                                        let base_img = base.clone();
                                        let evs_img = evs.clone();
                                        let sid_img = session_id_img.clone();
                                        tokio::spawn(async move {
                                            let rpc = Rpc::new(&rpc_img, &base_img);
                                            let payload = json!({
                                                "request": { "sessionId": sid_img, "attachmentId": att_id }
                                            });
                                            if let Ok(resp) = rpc.call("session/attachment", payload).await {
                                                if let Some(b64) = resp.get("data").and_then(|d| d.as_str()) {
                                                    let _ = evs_img.send(Ev::ImageB64(b64.to_string())).await;
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        if event_type == Some("assistant/message") {
                            let _ = evs.send(Ev::StreamEnd).await;
                        }
                        if event_type == Some("turn/start") {
                            let _ = evs.send(Ev::AgentRunning(true)).await;
                        }
                        if event_type == Some("turn/end") {
                            let _ = evs.send(Ev::AgentRunning(false)).await;
                        }
                        if !skip_echo {
                            let _ = evs.send(Ev::Event(value.clone())).await;
                        }
                    }
                }
            }
        }
    }

}

async fn refresh_sessions(rpc: &Rpc<'_>, evs: &Sender<Ev>) {
    match rpc.call("session/list", json!({ "_request": {} })).await {
        Ok(value) => {
            let rows = value
                .get("items")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            let id = item.get("sessionId")?.as_str()?.to_string();
                            let cwd = item
                                .get("cwd")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let title = item
                                .get("title")
                                .and_then(|v| v.as_str())
                                .or_else(|| {
                                    item.pointer("/projections/values/title")
                                        .and_then(|v| v.as_str())
                                })
                                .or_else(|| item.pointer("/projections/title").and_then(|v| v.as_str()))
                                .unwrap_or("")
                                .to_string();
                            let blank = item
                                .get("blank")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let updated_at = item
                                .get("updatedAt")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let running = item
                                .get("running")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            Some(SessionRow {
                                id,
                                cwd,
                                title,
                                running,
                                blank,
                                updated_at,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            save_sessions_cache(&rows);
            let _ = evs.send(Ev::Sessions(rows)).await;
        }
        Err(err) => {
            let _ = evs.send(Ev::Error(format!("session/list failed: {err}"))).await;
        }
    }
}

/// host 在每轮注入的上下文，不是用户输入。
fn is_injected(text: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "<system-reminder>",
        "Current runtime context",
        "Current working directory:",
        "Time sampled while preparing turn",
        "Elapsed since the preceding model-visible message",
    ];
    MARKERS.iter().any(|m| text.contains(m))
}

fn extract_text(data: &Value) -> String {
    let mut out = String::new();
    if let Some(content) = find_content(data) {
        push_blocks(content, &mut out);
    }
    out
}

/// 在事件 data 里找第一个 `content` 键（消息体的规范位置）。
fn find_content(value: &Value) -> Option<&Value> {
    let mut queue: Vec<&Value> = vec![value];
    while let Some(node) = queue.pop() {
        match node {
            Value::Object(map) => {
                if let Some(content) = map.get("content") {
                    return Some(content);
                }
                for child in map.values() {
                    if child.is_object() || child.is_array() {
                        queue.push(child);
                    }
                }
            }
            Value::Array(items) => {
                for child in items {
                    if child.is_object() || child.is_array() {
                        queue.push(child);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn push_blocks(node: &Value, out: &mut String) {
    match node {
        Value::Array(items) => {
            for item in items {
                push_blocks(item, out);
            }
        }
        Value::Object(map) => {
            let kind = map.get("type").and_then(|t| t.as_str());
            if kind != Some("text") {
                // reasoning / tool_use / image 等块不当作正文
                return;
            }
            if let Some(text) = map.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        Value::String(text) => {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Block {
    User { uid: u64, text: String },
    Assistant { uid: u64, text: String },
    Reasoning { uid: u64, text: String, expanded: bool },
    Tool {
        uid: u64,
        call_id: String,
        name: String,
        summary: String,
        args: String,
        output: Option<String>,
        is_error: bool,
        running: bool,
        expanded: bool,
    },
    TurnDivider { turn: i64 },
    Image(Arc<RenderImage>),
    Notice(String),
}

#[derive(Clone, Default)]
struct StreamState {
    text: String,
    reasoning: String,
}

#[derive(Clone)]
struct QuestionOption {
    label: String,
    description: Option<String>,
}

#[derive(Clone)]
struct QuestionItem {
    id: String,
    header: Option<String>,
    question: String,
    detail: Option<String>,
    options: Vec<QuestionOption>,
    multi: bool,
}

#[derive(Clone)]
struct PendingQuestion {
    event_id: String,
    call_id: Option<String>,
    questions: Vec<QuestionItem>,
    selected: Vec<Vec<String>>,
}

static NEXT_BLOCK_UID: AtomicU64 = AtomicU64::new(1);

fn block_uid() -> u64 {
    NEXT_BLOCK_UID.fetch_add(1, Ordering::SeqCst)
}

struct Root {
    cmds: Sender<Cmd>,
    evs: Receiver<Ev>,
    host: String,
    sessions: Vec<SessionRow>,
    selected: Option<String>,
    spaces: Vec<Space>,
    current_space: Option<String>,
    show_switcher: bool,
    show_sessions: bool,
    followed_once: bool,
    restored_ui: bool,
    current_model: String,
    current_provider: String,
    context_window: Option<u64>,
    git_cache_cwd: String,
    git_cache: String,
    show_models: bool,
    model_loading: bool,
    model_catalog: Value,
    items: Vec<Block>,
    term: Option<Term>,
    term_screen: Vec<Vec<Span>>,
    term_focus: FocusHandle,
    show_terminal: bool,
    streaming: Option<StreamState>,
    input: Entity<InputState>,
    question_input: Entity<InputState>,
    question: Option<PendingQuestion>,
    show_settings: bool,
    dsh_version: Option<String>,
    dsh_bundled: bool,
    dsh_busy: bool,
    dsh_status: Option<String>,
    scroll: ScrollHandle,
    error: Option<String>,
    connected: bool,
    ever_connected: bool,
    agent_running: bool,
    approval: Option<(String, String)>,
    pending_clear: bool,
    pending_question_clear: bool,
    _enter_sub: Subscription,
}

impl Root {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        eprintln!("[ui] root created");
        window.set_window_title("OMD");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("What are we cooking?"));
        let question_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("自定义回答（可选）"));
        // 启动即聚焦输入框
        input.read(cx).focus_handle(cx).focus(window, cx);

        let enter_sub = cx.subscribe(&input, |this, input_entity, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                let v = input_entity.read(cx).value();
                eprintln!("[ui] TYPING -> value is now {:?}", v);
            }
            if let InputEvent::PressEnter { shift, .. } = ev {
                let value = input_entity.read(cx).value();
                eprintln!("[ui] PressEnter! value={value:?} shift={shift}");
                let shift = *shift;
                if !shift {
                    this.send_prompt(cx);
                }
            }
        });
        let (cmd_tx, cmd_rx) = async_channel::unbounded::<Cmd>();
        let (ev_tx, ev_rx) = async_channel::unbounded::<Ev>();
        spawn_worker(cmd_rx, ev_tx);

        let evs = ev_rx.clone();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            eprintln!("[ui] event loop started");
            while let Ok(ev) = evs.recv().await {
                eprintln!("[ui] event received");
                let ok = this.update(cx, |root, cx| {
                    root.apply(ev);
                    cx.notify();
                    // 后台数据到达时强制请求重绘：Wayland 下被遮挡/未激活的窗口
                    // 没有持续的 frame callback，仅 notify 可能不触发绘制
                    cx.refresh_windows();
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        let (spaces, current_space, selected, saved_model, saved_provider) = load_ui_state();
        let restored_ui = !spaces.is_empty() || current_space.is_some();
        let model_catalog = load_model_cache().unwrap_or(Value::Null);
        Self {
            cmds: cmd_tx,
            evs: ev_rx,
            host: "starting dsh…".into(),
            sessions: Vec::new(),
            selected,
            spaces,
            current_space,
            show_switcher: false,
            show_sessions: false,
            followed_once: false,
            restored_ui,
            current_model: if saved_model.is_empty() {
                "DFModel".to_string()
            } else {
                saved_model
            },
            current_provider: saved_provider,
            context_window: None,
            git_cache_cwd: String::new(),
            git_cache: String::new(),
            show_models: false,
            model_loading: false,
            model_catalog,
            items: Vec::new(),
            term: Term::new(100, 30),
            term_screen: Vec::new(),
            term_focus: cx.focus_handle(),
            show_terminal: false,
            streaming: None,
            input,
            question_input,
            question: None,
            show_settings: false,
            dsh_version: None,
            dsh_bundled: false,
            dsh_busy: false,
            dsh_status: None,
            scroll: ScrollHandle::new(),
            error: None,
            connected: false,
            ever_connected: false,
            agent_running: false,
            approval: None,
            pending_clear: false,
            pending_question_clear: false,
            _enter_sub: enter_sub,
        }
    }

    fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::HostReady(base) => {
                eprintln!("[ui] host ready: {base}");
                eprintln!("[ui] AUTOTERM check: {:?}", std::env::var("DSH_GUI_AUTOTERM"));
                if std::env::var("DSH_GUI_AUTOTERM").is_ok() {
                    eprintln!("[ui] AUTOTERM firing");
                    self.show_terminal = true;
                    let r = self.cmds.send_blocking(Cmd::NewTerminal);
                    eprintln!("[ui] AUTOTERM sent: {r:?}");
                }
                self.host = base;
                self.error = None;
                let _ = self.cmds.send_blocking(Cmd::ModelCatalog);
                // A stray startup click must not leave the settings sheet open.
                self.show_settings = false;
            }
            Ev::ModelCatalog(value) => {
                save_model_cache(&value);
                self.model_catalog = value;
                self.model_loading = false;
            }
            Ev::ModelSelected { provider, model } => {
                self.current_provider = provider;
                self.current_model = model;
                self.show_models = false;
                self.save_state();
            }
            Ev::Sessions(rows) => {
                eprintln!("[ui] sessions loaded: {}", rows.len());
                let mut next_spaces: Vec<Space> = Vec::new();
                for row in &rows {
                    let cwd = if row.cwd.trim().is_empty() {
                        "/".to_string()
                    } else {
                        row.cwd.clone()
                    };
                    let id = space_id_for(&cwd);
                    if next_spaces.iter().any(|space| space.id == id) {
                        continue;
                    }
                    let old = self.spaces.iter().find(|space| space.id == id);
                    let mut tabs = old.map(|space| space.tabs.clone()).unwrap_or_default();
                    tabs.retain(|tab| rows.iter().any(|row| &row.id == tab));
                    let active = old
                        .and_then(|space| space.active.clone())
                        .filter(|tab| tabs.contains(tab));
                    next_spaces.push(Space {
                        id,
                        name: space_name(&cwd),
                        cwd: normalize_cwd(&cwd),
                        tabs,
                        active,
                    });
                }
                self.spaces = next_spaces;

                if self
                    .current_space
                    .as_ref()
                    .map_or(true, |id| !self.spaces.iter().any(|space| &space.id == id))
                {
                    self.current_space = self.spaces.first().map(|space| space.id.clone());
                }

                if self
                    .selected
                    .as_deref()
                    .is_some_and(|id| !rows.iter().any(|row| row.id == id))
                {
                    self.selected = None;
                }

                // Keep the persisted/selected tab attached to its space.
                if let Some(selected) = self.selected.clone() {
                    if let Some(row) = rows.iter().find(|row| row.id == selected) {
                        let space_id = space_id_for(&row.cwd);
                        if let Some(space) =
                            self.spaces.iter_mut().find(|space| space.id == space_id)
                        {
                            if !space.tabs.iter().any(|tab| tab == &selected) {
                                space.tabs.push(selected.clone());
                            }
                            space.active = Some(selected.clone());
                        }
                        self.current_space = Some(space_id);
                    }
                }

                // Restore the active tab of the current space.
                if self.selected.is_none() {
                    if let Some(space_id) = self.current_space.clone() {
                        if let Some(space) =
                            self.spaces.iter().find(|space| space.id == space_id)
                        {
                            if let Some(active) =
                                space.active.clone().or_else(|| space.tabs.first().cloned())
                            {
                                self.selected = Some(active.clone());
                                if let Some(space) =
                                    self.spaces.iter_mut().find(|space| space.id == space_id)
                                {
                                    space.active = Some(active);
                                }
                            }
                        }
                    }
                }

                // First launch: pick the most useful recent conversation.
                if self.selected.is_none() && !self.followed_once && !self.restored_ui {
                    let preferred = rows
                        .iter()
                        .filter(|row| !row.blank && !row.title.trim().is_empty())
                        .max_by_key(|row| row.updated_at)
                        .or_else(|| {
                            rows.iter()
                                .filter(|row| !row.blank)
                                .max_by_key(|row| row.updated_at)
                        })
                        .or_else(|| rows.first());
                    if let Some(row) = preferred {
                        let space_id = space_id_for(&row.cwd);
                        self.ensure_space(&row.cwd);
                        if let Some(space) =
                            self.spaces.iter_mut().find(|space| space.id == space_id)
                        {
                            if !space.tabs.iter().any(|tab| tab == &row.id) {
                                space.tabs.push(row.id.clone());
                            }
                            space.active = Some(row.id.clone());
                        }
                        self.current_space = Some(space_id);
                        self.selected = Some(row.id.clone());
                    }
                }

                if !self.followed_once {
                    if let Some(selected) = self.selected.clone() {
                        self.followed_once = true;
                        let _ = self.cmds.send_blocking(Cmd::Follow(selected));
                    }
                }

                if std::env::var("DSH_GUI_AUTOIMAGE").is_ok() {
                    let _ = self.cmds.send_blocking(Cmd::NewSession { cwd: None });
                }
                self.sessions = rows;
                self.save_state();
            }
            Ev::FollowStart(session_id) => {
                eprintln!("[ui] follow {session_id}");
                let cwd = self
                    .sessions
                    .iter()
                    .find(|row| row.id == session_id)
                    .map(|row| row.cwd.clone())
                    .or_else(|| {
                        self.current_space
                            .as_ref()
                            .and_then(|space_id| {
                                self.spaces.iter().find(|space| &space.id == space_id)
                            })
                            .map(|space| space.cwd.clone())
                    })
                    .unwrap_or_else(|| "/".to_string());
                let space_id = self.ensure_space(&cwd);
                if let Some(space) = self.spaces.iter_mut().find(|space| space.id == space_id) {
                    if !space.tabs.iter().any(|tab| tab == &session_id) {
                        space.tabs.push(session_id.clone());
                    }
                    space.active = Some(session_id.clone());
                }
                self.current_space = Some(space_id);
                self.selected = Some(session_id);
                self.followed_once = true;
                self.save_state();
                self.items.clear();
                self.streaming = None;
                self.question = None;
                self.approval = None;
                self.error = None;
            }
            Ev::Event(value) => {
                let stick = self.at_bottom();
                self.handle_event(&value);
                if stick {
                    self.scroll.scroll_to_bottom();
                }
            }
            Ev::Line(line) => {
                if self.items.len() < 40 {
                    eprintln!("[ui] line: {}", line.chars().take(100).collect::<String>());
                }
                let stick = self.at_bottom();
                self.push_block(Block::Notice(line));
                if stick {
                    self.scroll.scroll_to_bottom();
                }
            }
            Ev::ImageB64(b64) => {
                match decode_image(&b64) {
                    Some(img) => {
                        eprintln!("[ui] image decoded {:?}x{:?}", img.size(0).width, img.size(0).height);
                        self.push_block(Block::Image(Arc::new(img)));
                    }
                    None => self.push_block(Block::Notice("!! 图片解码失败".into())),
                }
            }
            Ev::StreamStart => {
                eprintln!("[ui] stream start");
                self.streaming.get_or_insert_with(StreamState::default);
            }
            Ev::StreamDelta(delta) => {
                let state = self.streaming.get_or_insert_with(StreamState::default);
                if state.text.len() < 3 {
                    eprintln!("[ui] stream delta: {delta:?}");
                }
                state.text.push_str(&delta);
                if self.at_bottom() {
                    self.scroll.scroll_to_bottom();
                }
            }
            Ev::ReasoningDelta(delta) => {
                let state = self.streaming.get_or_insert_with(StreamState::default);
                state.reasoning.push_str(&delta);
                if self.at_bottom() {
                    self.scroll.scroll_to_bottom();
                }
            }
            Ev::StreamEnd => {
                eprintln!("[ui] stream end");
                self.streaming = None;
            }
            Ev::Connection(up) => {
                self.connected = up;
                if up {
                    self.ever_connected = true;
                    self.error = None;
                }
            }
            Ev::TerminalOutput(data) => {
                if let Some(t) = self.term.as_mut() {
                    t.feed(&data);
                    self.term_screen = t.render_rows();
                }
            }
            Ev::TerminalScreen(_screen) => {}
            Ev::AgentRunning(running) => {
                self.agent_running = running;
            }
            Ev::Approval { event_id, label } => {
                self.approval = Some((event_id, label));
                self.scroll.scroll_to_bottom();
            }
            Ev::ApprovalGone => {
                self.approval = None;
            }
            Ev::Question { event_id, request } => {
                if let Some(q) = parse_question(event_id, &request) {
                    eprintln!("[ui] question with {} item(s)", q.questions.len());
                    self.question = Some(q);
                    self.pending_question_clear = true;
                    self.scroll.scroll_to_bottom();
                }
            }
            Ev::QuestionGone => {
                self.question = None;
            }
            Ev::DshInfo { version, bundled } => {
                self.dsh_version = Some(version);
                self.dsh_bundled = bundled;
            }
            Ev::DshUpdate { done, message } => {
                self.dsh_busy = !done;
                self.dsh_status = Some(message);
            }
            Ev::Error(err) => {
                self.error = Some(err.clone());
                self.push_block(Block::Notice(format!("!! {err}")));
            }
        }
    }

    fn session_title(&self, id: &str) -> String {
        self.sessions
            .iter()
            .find(|row| row.id == id)
            .map(|row| {
                let title = if row.title.trim().is_empty() {
                    short_cwd(&row.cwd)
                } else {
                    row.title.clone()
                };
                title.chars().take(26).collect()
            })
            .unwrap_or_else(|| "会话".to_string())
    }

    fn ensure_space(&mut self, cwd: &str) -> String {
        let id = space_id_for(cwd);
        if !self.spaces.iter().any(|space| space.id == id) {
            self.spaces.push(Space {
                id: id.clone(),
                name: space_name(cwd),
                cwd: normalize_cwd(cwd),
                tabs: Vec::new(),
                active: None,
            });
        }
        id
    }

    fn space_running(&self, space: &Space) -> bool {
        self.sessions
            .iter()
            .any(|row| space_id_for(&row.cwd) == space.id && row.running)
    }

    fn save_state(&self) {
        let path = ui_state_path();
        let spaces: Vec<Value> = self
            .spaces
            .iter()
            .map(|space| {
                json!({
                    "id": space.id,
                    "name": space.name,
                    "cwd": space.cwd,
                    "tabs": space.tabs,
                    "active": space.active,
                })
            })
            .collect();
        let value = json!({
            "spaces": spaces,
            "current_space": self.current_space,
            "selected": self.selected,
            "model": self.current_model,
            "provider": self.current_provider,
        });
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let Ok(bytes) = serde_json::to_vec_pretty(&value) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }

    fn open_tab(&mut self, id: String, cx: &mut Context<Self>) {
        let cwd = self
            .sessions
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.cwd.clone())
            .or_else(|| {
                self.current_space
                    .as_ref()
                    .and_then(|space_id| self.spaces.iter().find(|space| &space.id == space_id))
                    .map(|space| space.cwd.clone())
            })
            .unwrap_or_else(|| "/".to_string());
        let space_id = self.ensure_space(&cwd);
        if let Some(space) = self.spaces.iter_mut().find(|space| space.id == space_id) {
            if !space.tabs.iter().any(|tab| tab == &id) {
                space.tabs.push(id.clone());
            }
            space.active = Some(id.clone());
        }
        let changed = self.selected.as_deref() != Some(id.as_str());
        self.current_space = Some(space_id);
        self.selected = Some(id.clone());
        self.show_switcher = false;
        self.show_sessions = false;
        self.followed_once = true;
        if changed {
            let _ = self.cmds.send_blocking(Cmd::Follow(id));
        }
        self.save_state();
        cx.notify();
    }

    fn close_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(space_index) = self
            .spaces
            .iter()
            .position(|space| space.tabs.iter().any(|tab| tab == id))
        else {
            return;
        };
        let mut next = None;
        let mut space_now_empty = false;
        {
            let space = &mut self.spaces[space_index];
            let Some(position) = space.tabs.iter().position(|tab| tab == id) else {
                return;
            };
            space.tabs.remove(position);
            if space.active.as_deref() == Some(id) {
                if let Some(tab) = space
                    .tabs
                    .get(position)
                    .or_else(|| space.tabs.get(position.saturating_sub(1)))
                    .cloned()
                {
                    space.active = Some(tab.clone());
                    next = Some(tab);
                } else {
                    space.active = None;
                    space_now_empty = true;
                }
            }
        }
        if let Some(next) = next {
            self.selected = Some(next.clone());
            let _ = self.cmds.send_blocking(Cmd::Follow(next));
        } else if space_now_empty && self.selected.as_deref() == Some(id) {
            self.selected = None;
            self.items.clear();
            self.streaming = None;
        }
        self.save_state();
        cx.notify();
    }

    fn switch_space(&mut self, id: String, cx: &mut Context<Self>) {
        let active = self
            .spaces
            .iter()
            .find(|space| space.id == id)
            .and_then(|space| space.active.clone().or_else(|| space.tabs.first().cloned()));
        if let Some(active) = active {
            if let Some(space) = self.spaces.iter_mut().find(|space| space.id == id) {
                space.active = Some(active.clone());
            }
            self.current_space = Some(id);
            self.show_switcher = false;
            self.show_sessions = false;
            self.selected = Some(active.clone());
            self.followed_once = true;
            let _ = self.cmds.send_blocking(Cmd::Follow(active));
            self.save_state();
            cx.notify();
            return;
        }

        // The space has no open tab yet: adopt its most recent conversation.
        let best = self
            .sessions
            .iter()
            .filter(|row| space_id_for(&row.cwd) == id)
            .filter(|row| !row.blank && !row.title.trim().is_empty())
            .max_by_key(|row| row.updated_at)
            .or_else(|| {
                self.sessions
                    .iter()
                    .filter(|row| space_id_for(&row.cwd) == id)
                    .filter(|row| !row.blank)
                    .max_by_key(|row| row.updated_at)
            })
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|row| space_id_for(&row.cwd) == id)
            })
            .cloned();
        if let Some(row) = best {
            self.open_tab(row.id, cx);
            return;
        }

        self.current_space = Some(id);
        self.show_switcher = false;
        self.show_sessions = false;
        self.selected = None;
        self.items.clear();
        self.streaming = None;
        self.save_state();
        cx.notify();
    }

    fn push_block(&mut self, block: Block) {
        self.items.push(block);
        if self.items.len() > 800 {
            let drop = self.items.len() - 800;
            self.items.drain(0..drop);
        }
    }

    /// Whether the transcript is parked at the bottom (including a fresh,
    /// not-yet-scrollable one). Used to avoid yanking the user out of history.
    fn at_bottom(&self) -> bool {
        self.scroll.offset().y + self.scroll.max_offset().y <= px(12.)
    }

    fn handle_event(&mut self, frame: &Value) {
        if let Some(config) = frame.pointer("/data/header/config") {
            if let Some(provider) = config.get("provider").and_then(|v| v.as_str()) {
                self.current_provider = provider.to_string();
            }
            if let Some(model) = config.get("model").and_then(|v| v.as_str()) {
                self.current_model = model.to_string();
            }
        }
        if let Some(window) = frame.pointer("/data/contextWindow").and_then(|v| v.as_u64()) {
            self.context_window = Some(window);
        }
        if frame.get("type").and_then(|v| v.as_str()) == Some("model/selection") {
            if let Some(provider) = frame.pointer("/data/provider").and_then(|v| v.as_str()) {
                self.current_provider = provider.to_string();
            }
            if let Some(model) = frame.pointer("/data/model").and_then(|v| v.as_str()) {
                self.current_model = model.to_string();
            }
        }
        // 重连后 follow 会重新发 snapshot；清掉旧 transcript 再重放，避免重复。
        if frame.get("type").and_then(|v| v.as_str()) == Some("snapshot") {
            self.items.clear();
            self.streaming = None;
            self.scroll.scroll_to_bottom();
        }
        // baseline / page: records 里是一条条 SessionEventEntry
        if let Some(records) = frame.get("records").and_then(|v| v.as_array()) {
            for record in records {
                self.handle_event(record);
            }
            return;
        }
        let Some(event) = frame.get("event") else {
            return;
        };
        let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        match kind {
            "user/message" => {
                let text = extract_text(&data);
                let text = text.trim();
                if text.is_empty() || is_injected(text) {
                    return;
                }
                self.push_block(Block::User {
                    uid: block_uid(),
                    text: text.to_string(),
                });
            }
            "assistant/message" => {
                let mut pushed = false;
                if let Some(content) = data.pointer("/message/content").and_then(|v| v.as_array()) {
                    for block in content {
                        match block.get("type").and_then(|v| v.as_str()) {
                            Some("reasoning") => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    if !text.trim().is_empty() {
                                        self.push_block(Block::Reasoning {
                                            uid: block_uid(),
                                            text: text.to_string(),
                                            expanded: false,
                                        });
                                        pushed = true;
                                    }
                                }
                            }
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    if !text.trim().is_empty() {
                                        self.push_block(Block::Assistant {
                                            uid: block_uid(),
                                            text: text.to_string(),
                                        });
                                        pushed = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !pushed {
                    let text = extract_text(&data);
                    if !text.trim().is_empty() {
                        self.push_block(Block::Assistant {
                            uid: block_uid(),
                            text: text.trim().to_string(),
                        });
                    }
                }
                self.streaming = None;
            }
            "tool/call" => {
                let Some(call_id) = data.get("callId").and_then(|v| v.as_str()) else {
                    return;
                };
                let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                let arguments = data.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                let (summary, pretty) = tool_args(name, arguments);
                self.push_block(Block::Tool {
                    uid: block_uid(),
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    summary,
                    args: pretty,
                    output: None,
                    is_error: false,
                    running: true,
                    expanded: false,
                });
            }
            "tool/result" => {
                let call_id = data
                    .pointer("/message/toolCallId")
                    .and_then(|v| v.as_str())
                    .or_else(|| data.get("toolCallId").and_then(|v| v.as_str()))
                    .unwrap_or("");
                if call_id.is_empty() {
                    return;
                }
                let output = extract_text(&data);
                let is_error = data
                    .pointer("/message/isError")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    || data.get("error").map(|v| !v.is_null()).unwrap_or(false);
                if let Some(pos) = self.items.iter().rposition(
                    |b| matches!(b, Block::Tool { call_id: c, .. } if c == call_id),
                ) {
                    if let Block::Tool {
                        output: out,
                        is_error: err,
                        running,
                        ..
                    } = &mut self.items[pos]
                    {
                        *out = Some(output.trim().to_string());
                        *err = is_error;
                        *running = false;
                    }
                }
            }
            "turn/start" => {
                let turn = data.get("turn").and_then(|v| v.as_i64()).unwrap_or(0);
                if turn > 1 {
                    self.push_block(Block::TurnDivider { turn });
                }
            }
            "turn/end" => {
                let reason = data
                    .pointer("/reason/kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("completed");
                if reason != "completed" {
                    self.push_block(Block::Notice(format!("本轮结束：{reason}")));
                }
            }
            "session/title" => {
                let title = data.get("title").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(selected) = self.selected.as_deref() {
                    if let Some(row) = self.sessions.iter_mut().find(|r| r.id == selected) {
                        row.title = title.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    fn toggle_block(&mut self, uid: u64) {
        for block in self.items.iter_mut() {
            match block {
                Block::Reasoning {
                    uid: u,
                    expanded,
                    ..
                }
                | Block::Tool {
                    uid: u,
                    expanded,
                    ..
                } if *u == uid => {
                    *expanded = !*expanded;
                    break;
                }
                _ => {}
            }
        }
    }

    fn toggle_option(&mut self, question_index: usize, label: String) {
        let Some(q) = self.question.as_mut() else {
            return;
        };
        let Some(item) = q.questions.get(question_index) else {
            return;
        };
        let multi = item.multi;
        let Some(selected) = q.selected.get_mut(question_index) else {
            return;
        };
        if multi {
            if let Some(pos) = selected.iter().position(|s| s == &label) {
                selected.remove(pos);
            } else {
                selected.push(label);
            }
        } else if selected.first() == Some(&label) {
            selected.clear();
        } else {
            *selected = vec![label];
        }
    }

    fn submit_question(&mut self, cx: &mut Context<Self>) {
        let Some(q) = self.question.clone() else {
            return;
        };
        let custom = self.question_input.read(cx).value().trim().to_string();
        let mut answers = Vec::with_capacity(q.questions.len());
        for (i, item) in q.questions.iter().enumerate() {
            let selected = q.selected.get(i).cloned().unwrap_or_default();
            let mut entry = json!({ "id": item.id, "selected": selected });
            if i == 0 && !custom.is_empty() {
                entry["custom"] = json!(custom);
            }
            answers.push(entry);
        }
        let answer = json!({ "answers": answers });
        let _ = self.cmds.send_blocking(Cmd::AnswerQuestion {
            event_id: q.event_id,
            answer,
        });
        self.question = None;
        self.pending_question_clear = true;
        cx.notify();
    }

    fn send_prompt(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        trace(format_args!("[ui] send_prompt text={text:?}"));
        if text.is_empty() {
            return;
        }
        self.send_text(text, cx);
    }

    fn send_text(&mut self, text: String, cx: &mut Context<Self>) {
        trace(format_args!("[ui] send_text {text:?}"));
        self.push_block(Block::User {
            uid: block_uid(),
            text: text.clone(),
        });
        self.scroll.scroll_to_bottom();
        self.pending_clear = true;
        let request_id = format!("omd-{}", uuid_like());
        let _ = self.cmds.send_blocking(Cmd::Prompt { text, request_id });
        cx.notify();
    }
}

fn parse_question(event_id: String, request: &Value) -> Option<PendingQuestion> {
    let questions = request.get("questions")?.as_array()?;
    let mut items = Vec::new();
    for q in questions {
        let Some(id) = q.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let options = q
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        Some(QuestionOption {
                            label: o.get("label")?.as_str()?.to_string(),
                            description: o
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        items.push(QuestionItem {
            id: id.to_string(),
            header: q
                .get("header")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            question: q
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            detail: q
                .get("detail")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            options,
            multi: q
                .get("multiSelect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    if items.is_empty() {
        return None;
    }
    let count = items.len();
    Some(PendingQuestion {
        event_id,
        call_id: request
            .pointer("/wait/callId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        questions: items,
        selected: vec![Vec::new(); count],
    })
}

fn tool_args(name: &str, raw: &str) -> (String, String) {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return (truncate_line(raw, 100), raw.to_string());
    };
    let picked = ["command", "file_path", "path", "pattern", "query", "url"]
        .iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()));
    let summary = match picked {
        Some(s) if !s.is_empty() => truncate_line(s, 100),
        _ => name.to_string(),
    };
    let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|_| raw.to_string());
    (summary, pretty)
}

fn truncate_line(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    let mut out: String = line.chars().take(n).collect();
    if line.chars().count() > n {
        out.push('…');
    }
    out
}

fn render_block(block: Block, cx: &mut Context<Root>) -> AnyElement {
    match block {
        Block::User { text, .. } => div()
            .w_full()
            .text_sm()
            .text_color(rgb(0x8fb7ff))
            .child(SharedString::from(format!("you: {text}")))
            .into_any_element(),
        Block::Assistant { uid, text } => TextView::markdown(
            SharedString::from(format!("md-{uid}")),
            SharedString::from(text),
        )
        .w_full()
        .max_w(px(860.))
        .selectable(true)
        .code_block_actions(|block: &gpui_kit::base::text::CodeBlock, _window, cx| {
            let code = block.code().to_string();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            use std::hash::{Hash as _, Hasher as _};
            code.hash(&mut hasher);
            let id = format!("copy-{:x}", hasher.finish());
            let code_for_click = code.clone();
            let label = match block.lang() {
                Some(lang) => format!("{lang} · 复制"),
                None => "复制".to_string(),
            };
            div()
                .id(SharedString::from(id))
                .px(px(6.))
                .py(px(1.))
                .rounded_sm()
                .bg(rgb(0x2a2a33))
                .text_xs()
                .cursor_pointer()
                .child(SharedString::from(label))
                .on_click(move |_ev, _win, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(code_for_click.clone()));
                })
        })
        .into_any_element(),
        Block::Reasoning {
            uid,
            text,
            expanded,
        } => {
            let label = format!(
                "思考 · {} 字 {}",
                text.chars().count(),
                if expanded { "▾" } else { "▸" }
            );
            div()
                .flex()
                .flex_col()
                .w_full()
                .id(SharedString::from(format!("reason-{uid}")))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _ev, _win, _cx| this.toggle_block(uid)))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8d8da6))
                        .child(SharedString::from(label)),
                )
                .children(if expanded {
                    Some(
                        div()
                            .mt(px(4.))
                            .text_xs()
                            .text_color(rgb(0x9a9ab0))
                            .child(SharedString::from(text)),
                    )
                } else {
                    None
                })
                .into_any_element()
        }
        Block::Tool {
            uid,
            name,
            summary,
            args,
            output,
            is_error,
            running,
            expanded,
            ..
        } => {
            let (icon, color) = if running {
                ("●", 0xd8b45a)
            } else if is_error {
                ("✗", 0xff7b72)
            } else {
                ("✓", 0x6fbf73)
            };
            let header = format!(
                "{icon} {name}  {summary}  {}",
                if expanded { "▾" } else { "▸" }
            );
            let output = output.unwrap_or_default();
            div()
                .flex()
                .flex_col()
                .w_full()
                .p(px(8.))
                .rounded_sm()
                .bg(rgb(0x17171d))
                .id(SharedString::from(format!("tool-{uid}")))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _ev, _win, _cx| this.toggle_block(uid)))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(color))
                        .child(SharedString::from(header)),
                )
                .children(if expanded {
                    let mut details = vec![div()
                        .mt(px(6.))
                        .text_xs()
                        .font_family("monospace")
                        .text_color(rgb(0x8a8aa0))
                        .child(SharedString::from(args))
                        .into_any_element()];
                    if !output.is_empty() {
                        details.push(
                            div()
                                .id(SharedString::from(format!("tool-out-{uid}")))
                                .mt(px(6.))
                                .max_h(px(240.))
                                .overflow_y_scroll()
                                .text_xs()
                                .font_family("monospace")
                                .text_color(rgb(0xbfc7d5))
                                .child(SharedString::from(output))
                                .into_any_element(),
                        );
                    }
                    Some(div().flex().flex_col().children(details))
                } else {
                    None
                })
                .into_any_element()
        }
        Block::TurnDivider { turn } => div()
            .w_full()
            .py(px(4.))
            .text_xs()
            .text_color(rgb(0x5f5f72))
            .child(SharedString::from(format!("— 第 {turn} 轮 —")))
            .into_any_element(),
        Block::Image(source) => gpui_kit::img(source)
            .w(px(240.))
            .rounded_sm()
            .into_any_element(),
        Block::Notice(text) => div()
            .w_full()
            .text_xs()
            .text_color(rgb(0x8a8aa0))
            .child(SharedString::from(text))
            .into_any_element(),
    }
}

fn render_question(
    q: &PendingQuestion,
    input: &Entity<InputState>,
    cx: &mut Context<Root>,
) -> AnyElement {
    let mut body: Vec<AnyElement> = Vec::new();
    for (qi, item) in q.questions.iter().enumerate() {
        if let Some(header) = &item.header {
            body.push(
                div()
                    .text_xs()
                    .text_color(rgb(0x7f9bc4))
                    .child(SharedString::from(header.clone()))
                    .into_any_element(),
            );
        }
        body.push(
            div()
                .text_sm()
                .child(SharedString::from(item.question.clone()))
                .into_any_element(),
        );
        if let Some(detail) = &item.detail {
            body.push(
                div()
                    .text_xs()
                    .text_color(rgb(0x9a9ab0))
                    .child(SharedString::from(detail.clone()))
                    .into_any_element(),
            );
        }
        if item.multi {
            body.push(
                div()
                    .text_xs()
                    .text_color(rgb(0x7f9bc4))
                    .child("可多选")
                    .into_any_element(),
            );
        }
        if !item.options.is_empty() {
            let mut opts: Vec<AnyElement> = Vec::new();
            for opt in &item.options {
                let selected = q
                    .selected
                    .get(qi)
                    .map(|s| s.contains(&opt.label))
                    .unwrap_or(false);
                let label = opt.label.clone();
                let mut row = div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .px(px(8.))
                    .py(px(4.))
                    .rounded_sm()
                    .bg(if selected {
                        rgb(0x2c4a6e)
                    } else {
                        rgb(0x22242c)
                    })
                    .cursor_pointer()
                    .id(SharedString::from(format!("q-{qi}-{}", opt.label)))
                    .on_click(
                        cx.listener(move |this, _ev, _win, _cx| {
                            this.toggle_option(qi, label.clone())
                        }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .child(SharedString::from(opt.label.clone())),
                    );
                if let Some(description) = &opt.description {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8a8aa0))
                            .child(SharedString::from(description.clone())),
                    );
                }
                opts.push(row.into_any_element());
            }
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .children(opts)
                    .into_any_element(),
            );
        }
    }
    div()
        .flex()
        .flex_col()
        .w_full()
        .p(px(12.))
        .gap(px(6.))
        .rounded_sm()
        .bg(rgb(0x1b2433))
        .id("question")
        .child(
            div()
                .text_sm()
                .text_color(rgb(0x9fc6ff))
                .child("❓ agent 需要你的回答"),
        )
        .children(body)
        .child(Input::new(input))
        .child(
            div()
                .id("question-submit")
                .px(px(12.))
                .py(px(4.))
                .rounded_sm()
                .bg(rgb(0x2c4a6e))
                .text_sm()
                .cursor_pointer()
                .child("提交回答")
                .on_click(cx.listener(|this, _ev, _win, cx| this.submit_question(cx))),
        )
        .into_any_element()
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_clear {
            self.pending_clear = false;
            self.input
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.input.read(cx).focus_handle(cx).focus(window, cx);
        }
        if self.pending_question_clear {
            self.pending_question_clear = false;
            self.question_input
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
        let selected = self.selected.clone();
        let sessions = self.sessions.clone();
        let blocks = self.items.clone();
        let error = self.error.clone();
        let connected = self.connected;
        let ever_connected = self.ever_connected;
        let agent_running = self.agent_running;
        let show_terminal = self.show_terminal;
        let cmds = self.cmds.clone();
        let space_count = self.spaces.len();
        let current_space = self
            .current_space
            .as_ref()
            .and_then(|id| self.spaces.iter().find(|space| &space.id == id))
            .cloned();
        let space_label = current_space
            .as_ref()
            .map(|space| space.name.clone())
            .unwrap_or_else(|| "OMD".to_string());
        let space_cwd = current_space
            .as_ref()
            .map(|space| space.cwd.clone())
            .unwrap_or_else(|| "/".to_string());
        let space_id = current_space.as_ref().map(|space| space.id.clone());
        let tabs: Vec<(String, String, bool)> = current_space
            .as_ref()
            .map(|space| {
                space
                    .tabs
                    .iter()
                    .map(|id| {
                        (
                            id.clone(),
                            self.session_title(id),
                            selected.as_deref() == Some(id.as_str()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (status_dot, status_color) = match &error {
            Some(_) => ("●", 0xff7b72),
            None if connected => ("●", 0x6fbf73),
            None if ever_connected => ("●", 0xd8b45a),
            None => ("○", 0x8a8aa0),
        };

        let term_panel = if show_terminal {
            let screen = self.term_screen.clone();
            let term_focus = self.term_focus.clone();
            Some(
                div().flex().flex_col()
                    .id("term")
                    .w(px(560.))
                    .min_h(px(0.))
                    .bg(rgb(0x0a0a0c))
                    .overflow_y_scroll()
                    .p(px(8.))
                    .track_focus(&term_focus)
                    .on_mouse_down(MouseButton::Left, {
                        let term_focus = term_focus.clone();
                        move |_ev, win, _cx| {
                            term_focus.focus(win, _cx);
                        }
                    })
                    .children(screen.iter().map(|spans| {
                        div().flex().flex_row().children(spans.iter().map(|span| {
                            let mut el = div().text_sm().font_family("monospace").child(
                                SharedString::from(span.text.clone()),
                            );
                            if let Some(fg) = span.fg {
                                el = el.text_color(rgb(fg));
                            }
                            if let Some(bg) = span.bg {
                                el = el.bg(rgb(bg));
                            }
                            el
                        }))
                    })),
            )
        } else {
            None
        };

        let streaming_markdown = self
            .streaming
            .as_ref()
            .filter(|state| !state.text.trim().is_empty())
            .map(|state| state.text.clone());
        let streaming_reasoning = self
            .streaming
            .as_ref()
            .filter(|state| state.text.trim().is_empty() && !state.reasoning.trim().is_empty())
            .map(|_| "思考中…".to_string());

        let approval_card = self.approval.clone().map(|(event_id, label)| {
            let cmds_allow = cmds.clone();
            let cmds_reject = cmds.clone();
            let id_allow = event_id.clone();
            let id_reject = event_id.clone();
            div().flex().flex_col()
                .id("approval")
                .w_full()
                .p(px(10.))
                .mb(px(8.))
                .rounded_sm()
                .bg(rgb(0x3a2f1f))
                .gap(px(6.))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xffd479))
                        .child("⚠ 需要授权"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xd8c9a8))
                        .child(SharedString::from(label)),
                )
                .child(
                    div().flex().flex_row().gap(px(8.)).child(
                        div()
                            .id("allow")
                            .px(px(10.))
                            .py(px(4.))
                            .rounded_sm()
                            .bg(rgb(0x2d5f36))
                            .text_sm()
                            .cursor_pointer()
                            .child("允许")
                            .on_click(move |_ev, _win, _cx| {
                                let _ = cmds_allow.send_blocking(Cmd::Decide {
                                    event_id: id_allow.clone(),
                                    allow: true,
                                });
                            }),
                    )
                    .child(
                        div()
                            .id("reject")
                            .px(px(10.))
                            .py(px(4.))
                            .rounded_sm()
                            .bg(rgb(0x5f2d2d))
                            .text_sm()
                            .cursor_pointer()
                            .child("拒绝")
                            .on_click(move |_ev, _win, _cx| {
                                let _ = cmds_reject.send_blocking(Cmd::Decide {
                                    event_id: id_reject.clone(),
                                    allow: false,
                                });
                            }),
                    ),
                )
        });

        let question_card = self
            .question
            .as_ref()
            .map(|q| render_question(q, &self.question_input, cx));

        let empty = blocks.is_empty()
            && self.streaming.is_none()
            && self.question.is_none()
            && self.approval.is_none();
        let transcript = div().flex().flex_col()
            .id("transcript")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .gap(px(4.))
            .p(px(12.))
            .children(blocks.into_iter().map(|block| render_block(block, cx)))
            .children(streaming_markdown.map(|text| {
                div().flex().flex_col().gap(px(2.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x9fd0a0))
                            .child("agent"),
                    )
                    .child(
                        TextView::markdown("stream-md", SharedString::from(text))
                            .w_full()
                            .max_w(px(860.))
                            .selectable(false),
                    )
            }))
            .children(streaming_reasoning.map(|line| {
                div()
                    .text_sm()
                    .text_color(rgb(0x9fd0a0))
                    .child(SharedString::from(line))
            }))
            .children(if empty {
                Some(
                    div()
                        .w_full()
                        .py(px(90.))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_lg()
                                .text_color(rgb(0x777788))
                                .child(if self.selected.is_some() {
                                    "新会话"
                                } else if space_id.is_some() {
                                    "此空间没有打开的会话"
                                } else {
                                    "没有工作区"
                                }),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0x5f5f72))
                                .child(if self.selected.is_some() {
                                    "在下面输入消息开始"
                                } else if space_id.is_some() {
                                    "点击 ▦ 选择会话，或点 + 新建"
                                } else {
                                    "点击 + 新建会话"
                                }),
                        ),
                )
            } else {
                None
            })
            .children(question_card)
            .children(approval_card);

        let transcript_area = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(transcript);

        let can_send = !self.input.read(cx).value().trim().is_empty();
        if self.git_cache_cwd != space_cwd {
            self.git_cache = git_summary(&space_cwd).unwrap_or_default();
            self.git_cache_cwd = space_cwd.clone();
        }
        let git_text = self.git_cache.clone();
        let model_label = catalog_model_name(&self.model_catalog, &self.current_model)
            .unwrap_or_else(|| pretty_model(&self.current_model));
        let path_label = pretty_cwd(&space_cwd);
        let context_label = self
            .context_window
            .map(format_tokens)
            .unwrap_or_else(|| "1M".to_string());

        let typing = div()
            .w_full()
            .px(px(16.))
            .pt(px(2.))
            .pb(px(14.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(1200.))
                    .mx_auto()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .px(px(6.))
                            .child(div().text_xs().text_color(rgb(0x5f5f72)).child("ctx"))
                            .child(div().flex_1())
                            .children(if context_label.is_empty() {
                                None
                            } else {
                                Some(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x5f5f72))
                                        .child(SharedString::from(context_label)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .rounded(px(16.))
                            .bg(rgb(0x1e1e23))
                            .border_1()
                            .border_color(rgb(0x3a3a40))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(10.))
                                    .px(px(20.))
                                    .pt(px(16.))
                                    .pb(px(6.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .child(
                                                Input::new(&self.input)
                                                    .bordered(false)
                                                    .appearance(false)
                                                    .focus_bordered(false)
                                                    .text_lg(),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(10.))
                                    .px(px(12.))
                                    .pb(px(10.))
                                    .child(
                                        div()
                                            .id("model-pill")
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap(px(5.))
                                            .px(px(9.))
                                            .py(px(4.))
                                            .rounded_full()
                                            .bg(rgb(0x2a2a33))
                                            .cursor_pointer()
                                            .hover(|style| style.bg(rgb(0x32323c)))
                                            .child(div().text_xs().text_color(rgb(0x4f9cf0)).child("◌"))
                                            .child(div().text_sm().child(SharedString::from(model_label)))
                                            .child(div().text_xs().text_color(rgb(0x777788)).child("⌄"))
                                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                                this.show_models = !this.show_models;
                                                if this.show_models {
                                                    let empty = this
                                                        .model_catalog
                                                        .get("groups")
                                                        .and_then(|v| v.as_array())
                                                        .map_or(true, |groups| groups.is_empty());
                                                    if empty {
                                                        this.model_loading = true;
                                                        let _ = this.cmds.send_blocking(Cmd::ModelCatalog);
                                                    }
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap(px(4.))
                                            .text_xs()
                                            .text_color(rgb(0x8a8aa0))
                                            .child("▱")
                                            .child(SharedString::from(path_label)),
                                    )
                                    .children(if git_text.is_empty() {
                                        None
                                    } else {
                                        Some(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap(px(4.))
                                                .text_xs()
                                                .text_color(rgb(0xd6b45a))
                                                .child("⑂")
                                                .child(SharedString::from(git_text)),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x5f5f72))
                                            .child("scheduler: 0"),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .id("send")
                                            .w(px(34.))
                                            .h(px(34.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_full()
                                            .bg(if can_send {
                                                rgb(0x2f5f8a)
                                            } else {
                                                rgb(0x2a2a33)
                                            })
                                            .text_sm()
                                            .text_color(if can_send {
                                                rgb(0xe8e8f0)
                                            } else {
                                                rgb(0x8a8aa0)
                                            })
                                            .cursor_pointer()
                                            .child(if can_send { "↑" } else { "＋" })
                                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                                if !this.input.read(cx).value().trim().is_empty() {
                                                    this.send_prompt(cx);
                                                }
                                            })),
                                    ),
                            ),
                    ),
            );

        let settings_modal = if self.show_settings {
            let version = self
                .dsh_version
                .clone()
                .unwrap_or_else(|| "dev".to_string());
            let busy = self.dsh_busy;
            let status = self.dsh_status.clone();
            Some(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x000000cc))
                    .child(
                        div()
                            .w(px(460.))
                            .p(px(18.))
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .rounded_lg()
                            .bg(rgb(0x1b1b20))
                            .border_1()
                            .border_color(rgb(0x32323c))
                            .child(div().text_lg().child("设置"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x9a9ab0))
                                    .child("dsh 引擎"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .child(SharedString::from(format!("当前版本：v{version}"))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8a8aa0))
                                    .child(if self.dsh_bundled {
                                        "内置运行时（应用数据目录）"
                                    } else {
                                        "外部 dsh（开发模式）"
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8a8aa0))
                                    .child(if busy {
                                        "正在更新，请稍候…".to_string()
                                    } else {
                                        "更新会下载到应用数据目录，关闭并重新打开 App 后生效。"
                                            .to_string()
                                    }),
                            )
                            .children(status.map(|text| {
                                div()
                                    .text_xs()
                                    .text_color(rgb(0xd8b45a))
                                    .child(SharedString::from(text))
                            }))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .id("update-dsh")
                                            .px(px(12.))
                                            .py(px(6.))
                                            .rounded_sm()
                                            .bg(if busy {
                                                rgb(0x26262e)
                                            } else {
                                                rgb(0x2f5f8a)
                                            })
                                            .text_sm()
                                            .cursor_pointer()
                                            .child(if busy { "更新中…" } else { "更新到最新版" })
                                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                                if this.dsh_busy {
                                                    return;
                                                }
                                                this.dsh_busy = true;
                                                this.dsh_status =
                                                    Some("正在下载并安装 dsh…".into());
                                                let _ = this
                                                    .cmds
                                                    .send_blocking(Cmd::UpdateDsh);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("close-settings")
                                            .px(px(12.))
                                            .py(px(6.))
                                            .rounded_sm()
                                            .bg(rgb(0x2a2a33))
                                            .text_sm()
                                            .cursor_pointer()
                                            .child("关闭")
                                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                                this.show_settings = false;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    ),
            )
        } else {
            None
        };
        let model_groups: Vec<Value> = self
            .model_catalog
            .get("groups")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let model_picker = if self.show_models {
            Some(
                div()
                    .absolute()
                    .inset_0()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .bg(rgba(0x00000000))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _win, cx| {
                                    this.show_models = false;
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("model-picker")
                            .absolute()
                            .bottom(px(132.))
                            .left(px(16.))
                            .w(px(390.))
                            .max_h(px(420.))
                            .overflow_y_scroll()
                            .p(px(6.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .rounded_lg()
                            .bg(rgb(0x1b1b20))
                            .border_1()
                            .border_color(rgb(0x32323c))
                            .child(
                                div()
                                    .flex().flex_row().items_center()
                                    .px(px(8.)).py(px(4.))
                                    .child(div().text_sm().child("模型"))
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x5f5f72))
                                            .child(SharedString::from(catalog_model_name(&self.model_catalog, &self.current_model).unwrap_or_else(|| pretty_model(&self.current_model)))),
                                    ),
                            )
                            .children(if self.model_loading {
                                vec![div()
                                    .px(px(8.)).py(px(10.))
                                    .text_xs()
                                    .text_color(rgb(0x777788))
                                    .child("加载模型…")
                                    .into_any_element()]
                            } else if model_groups.is_empty() {
                                vec![div()
                                    .px(px(8.)).py(px(10.))
                                    .text_xs()
                                    .text_color(rgb(0x777788))
                                    .child("没有可用模型")
                                    .into_any_element()]
                            } else {
                                let mut rows = Vec::new();
                                for group in model_groups {
                                    let provider = group
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let provider_name = group
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&provider)
                                        .to_string();
                                    rows.push(
                                        div()
                                            .px(px(8.)).pt(px(8.)).pb(px(2.))
                                            .text_xs()
                                            .text_color(rgb(0x5f5f72))
                                            .child(SharedString::from(provider_name))
                                            .into_any_element(),
                                    );
                                    let Some(models) = group.get("models").and_then(|v| v.as_array()) else {
                                        continue;
                                    };
                                    for model in models {
                                        let id = model
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let name = model
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or(&id)
                                            .to_string();
                                        let description = model
                                            .get("description")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let active = self.current_model == id;
                                        let provider = provider.clone();
                                        let model_id = id.clone();
                                        rows.push(
                                            div()
                                                .id(SharedString::from(format!("model-{provider}-{id}")))
                                                .flex().flex_row().items_center().gap(px(6.))
                                                .px(px(8.)).py(px(6.))
                                                .rounded_sm()
                                                .bg(if active { rgb(0x2a2a33) } else { rgb(0x1b1b20) })
                                                .hover(|style| style.bg(rgb(0x24242c)))
                                                .cursor_pointer()
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _ev, _win, cx| {
                                                        this.show_models = false;
                                                        let _ = this.cmds.send_blocking(Cmd::SelectModel {
                                                            provider: provider.clone(),
                                                            model: model_id.clone(),
                                                        });
                                                        cx.notify();
                                                    }),
                                                )
                                                .child(
                                                    div()
                                                        .flex().flex_col().flex_1()
                                                        .child(div().text_sm().child(SharedString::from(name)))
                                                        .children(if description.is_empty() {
                                                            None
                                                        } else {
                                                            Some(
                                                                div()
                                                                    .text_xs()
                                                                    .text_color(rgb(0x777788))
                                                                    .child(SharedString::from(description)),
                                                            )
                                                        }),
                                                )
                                                .children(if active {
                                                    Some(
                                                        div()
                                                            .text_xs()
                                                            .text_color(rgb(0x6fbf73))
                                                            .child("✓"),
                                                    )
                                                } else {
                                                    None
                                                })
                                                .into_any_element(),
                                        );
                                    }
                                }
                                rows
                            }),
                    ),
            )
        } else {
            None
        };

        let switcher = if self.show_switcher {
            Some(
                div()
                    .absolute()
                    .inset_0()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .bg(rgba(0x00000000))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _win, cx| {
                                    this.show_switcher = false;
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("space-switcher-panel")
                            .absolute()
                            .top(px(44.))
                            .left(px(8.))
                            .w(px(460.))
                            .max_h(px(520.))
                            .overflow_y_scroll()
                            .p(px(6.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .rounded_lg()
                            .bg(rgb(0x1b1b20))
                            .border_1()
                            .border_color(rgb(0x32323c))
                            .child(
                                div()
                                    .flex().flex_row().items_center()
                                    .px(px(8.)).py(px(4.))
                                    .child(div().text_sm().text_color(rgb(0xe8e8f0)).child("切换空间"))
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x777788))
                                            .child(SharedString::from(format!("{space_count} 个"))),
                                    ),
                            )
                            .children(self.spaces.iter().map(|space| {
                                let id = space.id.clone();
                                let active = self.current_space.as_deref() == Some(space.id.as_str());
                                let running = self.space_running(space);
                                let tabs = space.tabs.len();
                                let sub = short_cwd(&space.cwd);
                                div()
                                    .id(SharedString::from(format!("space-{}", space.id)))
                                    .flex().flex_row().items_center().gap(px(8.))
                                    .px(px(8.)).py(px(6.))
                                    .rounded_sm()
                                    .bg(if active { rgb(0x2a2a33) } else { rgb(0x1b1b20) })
                                    .hover(|style| style.bg(rgb(0x24242c)))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _ev, _win, cx| {
                                        this.switch_space(id.clone(), cx);
                                    }))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(if running { rgb(0x6fbf73) } else { rgb(0x6f6f88) })
                                            .child(if running { "●" } else { "○" }),
                                    )
                                    .child(
                                        div()
                                            .flex().flex_col().flex_1()
                                            .child(
                                                div()
                                                    .flex().flex_row().items_center().gap(px(6.))
                                                    .child(div().text_sm().child(SharedString::from(space.name.clone())))
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(rgb(0x777788))
                                                            .child(SharedString::from(format!("{tabs} tabs"))),
                                                    )
                                                    .children(if active {
                                                        Some(
                                                            div()
                                                                .text_xs()
                                                                .text_color(rgb(0x6fbf73))
                                                                .child("current"),
                                                        )
                                                    } else {
                                                        None
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(0x777788))
                                                    .child(SharedString::from(sub)),
                                            ),
                                    )
                            })),
                    ),
            )
        } else {
            None
        };

        let session_picker = if self.show_sessions {
            let rows: Vec<SessionRow> = self
                .sessions
                .iter()
                .filter(|row| {
                    space_id
                        .as_deref()
                        .is_some_and(|space| space_id_for(&row.cwd) == space)
                })
                .cloned()
                .collect();
            Some(
                div()
                    .absolute()
                    .inset_0()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .bg(rgba(0x00000000))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _win, cx| {
                                    this.show_sessions = false;
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("session-picker-panel")
                            .absolute()
                            .top(px(44.))
                            .left(px(8.))
                            .w(px(460.))
                            .max_h(px(520.))
                            .overflow_y_scroll()
                            .p(px(6.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .rounded_lg()
                            .bg(rgb(0x1b1b20))
                            .border_1()
                            .border_color(rgb(0x32323c))
                            .child(
                                div()
                                    .flex().flex_row().items_center()
                                    .px(px(8.)).py(px(4.))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xe8e8f0))
                                            .child(SharedString::from(format!("会话 · {space_label}"))),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x777788))
                                            .child(SharedString::from(format!("{} 个", rows.len()))),
                                    ),
                            )
                            .children(if rows.is_empty() {
                                vec![div()
                                    .px(px(8.)).py(px(12.))
                                    .text_xs()
                                    .text_color(rgb(0x777788))
                                    .child("此空间还没有会话")
                                    .into_any_element()]
                            } else {
                                rows.into_iter()
                                    .map(|row| {
                                        let id = row.id.clone();
                                        let active = self.selected.as_deref() == Some(row.id.as_str());
                                        let title = if row.title.trim().is_empty() {
                                            short_cwd(&row.cwd)
                                        } else {
                                            row.title.chars().take(40).collect::<String>()
                                        };
                                        let sub = short_cwd(&row.cwd);
                                        div()
                                            .id(SharedString::from(format!("session-{}", row.id)))
                                            .flex().flex_row().items_center().gap(px(8.))
                                            .px(px(8.)).py(px(5.))
                                            .rounded_sm()
                                            .bg(if active { rgb(0x2a2a33) } else { rgb(0x1b1b20) })
                                            .hover(|style| style.bg(rgb(0x24242c)))
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                                this.open_tab(id.clone(), cx);
                                            }))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(if row.running { rgb(0x6fbf73) } else { rgb(0x6f6f88) })
                                                    .child(if row.running { "●" } else { "○" }),
                                            )
                                            .child(
                                                div()
                                                    .flex().flex_col().flex_1()
                                                    .child(div().text_sm().child(SharedString::from(title)))
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(rgb(0x777788))
                                                            .child(SharedString::from(sub)),
                                                    ),
                                            )
                                            .into_any_element()
                                    })
                                    .collect()
                            }),
                    ),
            )
        } else {
            None
        };

        let root = div().flex().flex_col()
            .id("root")
            .relative()
            .size_full()
            .bg(rgb(0x111114))
            .text_color(rgb(0xd8d8e0));
        // 键盘路由只在终端面板打开时挂（避免任何情况下干扰输入框）
        let root = if show_terminal {
            root.on_key_down(cx.listener(|this, ev: &KeyDownEvent, _win, cx| {
                if this.show_terminal && this.term_focus.contains_focused(_win, cx) {
                    if let Some(data) = key_to_bytes(&ev.keystroke) {
                        let _ = this.cmds.send_blocking(Cmd::TerminalInput(data));
                        cx.stop_propagation();
                    }
                }
            }))
        } else {
            root
        };
        root
            .child(
                div().flex().flex_col().w_full()
                    .child(
                        div().flex().flex_row()
                            .items_center()
                            .w_full()
                            .h(px(38.))
                            .px(px(8.))
                            .gap(px(6.))
                            .bg(rgb(0x1b1b20))
                            .child(
                                div()
                                    .id("session-switcher")
                                    .flex().flex_row().items_center().gap(px(3.))
                                    .px(px(6.)).py(px(3.))
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(0x2a2a33)))
                                    .on_click(cx.listener(|this, _ev, _win, cx| {
                                        this.show_switcher = !this.show_switcher;
                                        this.show_settings = false;
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xe8e8f0))
                                            .child(SharedString::from(space_label)),
                                    )
                                    .child(div().text_xs().text_color(rgb(0x8a8aa0)).child("⌄")),
                            )
                            .child(
                                div().w(px(1.)).h(px(16.)).bg(rgb(0x32323c)),
                            )
                            .child(
                                div().flex().flex_row().items_center().gap(px(3.))
                                    .max_w(px(720.))
                                    .overflow_hidden()
                                    .children(tabs.into_iter().map(|(id, title, active)| {
                                        let open_id = id.clone();
                                        let close_id = id.clone();
                                        div()
                                            .id(SharedString::from(format!("tab-{id}")))
                                            .flex().flex_row().items_center().gap(px(4.))
                                            .px(px(8.)).py(px(3.))
                                            .rounded_md()
                                            .bg(if active { rgb(0x2a2a33) } else { rgb(0x1b1b20) })
                                            .cursor_pointer()
                                            .hover(|style| style.bg(rgb(0x24242c)))
                                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                                this.open_tab(open_id.clone(), cx);
                                            }))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(if active {
                                                        rgb(0xe8e8f0)
                                                    } else {
                                                        rgb(0x9a9ab0)
                                                    })
                                                    .max_w(px(160.))
                                                    .overflow_hidden()
                                                    .child(SharedString::from(title)),
                                            )
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!("tab-close-{close_id}")))
                                                    .px(px(2.))
                                                    .text_xs()
                                                    .text_color(rgb(0x777788))
                                                    .cursor_pointer()
                                                    .child("×")
                                                    .on_click(cx.listener(move |this, _ev, _win, cx| {
                                                        cx.stop_propagation();
                                                        this.close_tab(&close_id, cx);
                                                    })),
                                            )
                                    })),
                            )
                            .child(
                                div()
                                    .id("new-tab")
                                    .w(px(22.)).h(px(22.))
                                    .flex().items_center().justify_center()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(0x9a9ab0))
                                    .hover(|style| style.bg(rgb(0x2a2a33)))
                                    .child("+")
                                    .on_click({
                                        let cmds = cmds.clone();
                                        let cwd = space_cwd.clone();
                                        move |_ev, _win, _cx| {
                                            let _ = cmds.send_blocking(Cmd::NewSession {
                                                cwd: Some(cwd.clone()),
                                            });
                                        }
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .min_h(px(24.))
                                    .on_mouse_down(MouseButton::Left, |_ev, win, _cx| {
                                        win.start_window_move();
                                    }),
                            )
                            .children(if agent_running {
                                let cmds = cmds.clone();
                                Some(
                                    div()
                                        .id("cancel")
                                        .w(px(24.)).h(px(22.))
                                        .flex().items_center().justify_center()
                                        .rounded_sm()
                                        .bg(rgb(0x5f2d2d))
                                        .text_xs()
                                        .cursor_pointer()
                                        .child("■")
                                        .on_click(move |_ev, _win, _cx| {
                                            let _ = cmds.send_blocking(Cmd::Cancel);
                                        }),
                                )
                            } else {
                                None
                            })
                            .child(
                                div()
                                    .id("toggle-terminal")
                                    .w(px(26.)).h(px(22.))
                                    .flex().items_center().justify_center()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_xs()
                                    .text_color(if show_terminal { rgb(0x9fc6ff) } else { rgb(0x9a9ab0) })
                                    .hover(|style| style.bg(rgb(0x2a2a33)))
                                    .child(">_")
                                    .on_click(cx.listener(|this, _ev, win, cx| {
                                        this.show_terminal = !this.show_terminal;
                                        if this.show_terminal && this.term.is_none() {
                                            this.term = Term::new(100, 30);
                                        }
                                        if this.show_terminal {
                                            let _ = this.cmds.send_blocking(Cmd::NewTerminal);
                                        } else {
                                            this.input.read(cx).focus_handle(cx).focus(win, cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("toggle-sidebar")
                                    .w(px(26.)).h(px(22.))
                                    .flex().items_center().justify_center()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_xs()
                                    .text_color(if self.show_sessions { rgb(0x9fc6ff) } else { rgb(0x9a9ab0) })
                                    .hover(|style| style.bg(rgb(0x2a2a33)))
                                    .child("▦")
                                    .on_click(cx.listener(|this, _ev, _win, cx| {
                                        this.show_sessions = !this.show_sessions;
                                        this.show_settings = false;
                                        cx.notify();
                                    })),
                            )
                            .children(if !connected {
                                Some(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(status_color))
                                        .child(status_dot),
                                )
                            } else {
                                None
                            })
                            .child(
                                div()
                                    .id("settings")
                                    .w(px(30.)).h(px(22.))
                                    .flex().items_center().justify_center()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(0xc8c8d4))
                                    .hover(|style| style.bg(rgb(0x32323c)))
                                    .child("⚙")
                                    .on_click(cx.listener(|this, _ev, _win, cx| {
                                        this.show_settings = true;
                                        this.show_switcher = false;
                                        cx.notify();
                                    })),
                            )
                            .child(window_button("win-min", "—", 0x32323c, |win, _cx| {
                                win.minimize_window();
                            }))
                            .child(window_button("win-max", "□", 0x32323c, |win, _cx| {
                                win.zoom_window();
                            }))
                            .child(window_button("win-close", "×", 0x7a3030, |win, _cx| {
                                win.remove_window();
                            })),
                    )
            )
            .child(
                div().flex().flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .children(term_panel)
                    .child(transcript_area),
            )
            .child(typing)
            .children(switcher)
            .children(session_picker)
            .children(model_picker)
            .children(settings_modal)
    }
}

fn window_button(
    id: &'static str,
    label: &'static str,
    hover_bg: u32,
    action: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(30.))
        .h(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .text_sm()
        .text_color(rgb(0xc8c8d4))
        .hover(move |style| style.bg(rgb(hover_bg)))
        .child(label)
        .on_click(move |_ev, win, cx| action(win, cx))
}

fn normalize_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn space_id_for(cwd: &str) -> String {
    normalize_cwd(cwd)
}

fn space_name(cwd: &str) -> String {
    let id = normalize_cwd(cwd);
    if id == "/" {
        "/".to_string()
    } else {
        id.rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&id)
            .to_string()
    }
}

fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OMD_DATA_DIR")
        .or_else(|_| std::env::var("DSH_GUI_DATA_DIR"))
    {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/share")
        });
    let primary = base.join("omd");
    if primary.exists() {
        primary
    } else {
        let legacy = base.join("dsh-gui");
        if legacy.exists() {
            legacy
        } else {
            primary
        }
    }
}

fn sessions_cache_path() -> PathBuf {
    data_dir().join("sessions-cache.json")
}

fn load_sessions_cache() -> Option<Vec<SessionRow>> {
    let text = std::fs::read_to_string(sessions_cache_path()).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let items = value.get("sessions")?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|item| {
                Some(SessionRow {
                    id: item.get("id")?.as_str()?.to_string(),
                    cwd: item
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    title: item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    // Cached running state is not trustworthy after a restart.
                    running: false,
                    blank: item.get("blank").and_then(|v| v.as_bool()).unwrap_or(false),
                    updated_at: item.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0),
                })
            })
            .collect(),
    )
}

fn save_sessions_cache(rows: &[SessionRow]) {
    let sessions: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "cwd": row.cwd,
                "title": row.title,
                "running": row.running,
                "blank": row.blank,
                "updated_at": row.updated_at,
            })
        })
        .collect();
    let value = json!({ "updated_at": now_millis(), "sessions": sessions });
    write_cache_json(&sessions_cache_path(), &value);
}

fn model_cache_path() -> PathBuf {
    data_dir().join("model-cache.json")
}

fn load_model_cache() -> Option<Value> {
    let text = std::fs::read_to_string(model_cache_path()).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("catalog").cloned()
}

fn save_model_cache(catalog: &Value) {
    let value = json!({ "updated_at": now_millis(), "catalog": catalog });
    write_cache_json(&model_cache_path(), &value);
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn write_cache_json(path: &std::path::Path, value: &Value) {
    let Some(parent) = path.parent() else { return };
    let _ = std::fs::create_dir_all(parent);
    let tmp = path.with_extension("tmp");
    let Ok(bytes) = serde_json::to_vec(value) else { return };
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn ui_state_path() -> PathBuf {
    data_dir().join("ui-state.json")
}

fn load_ui_state() -> (Vec<Space>, Option<String>, Option<String>, String, String) {
    let Ok(text) = std::fs::read_to_string(ui_state_path()) else {
        return (Vec::new(), None, None, String::new(), String::new());
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (Vec::new(), None, None, String::new(), String::new());
    };
    let spaces: Vec<Space> = value
        .get("spaces")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let cwd = item.get("cwd").and_then(|v| v.as_str())?;
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| space_id_for(cwd));
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| space_name(cwd));
                    let tabs: Vec<String> = item
                        .get("tabs")
                        .and_then(|v| v.as_array())
                        .map(|tabs| {
                            tabs.iter()
                                .filter_map(|tab| tab.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let active = item
                        .get("active")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .filter(|id| tabs.contains(id));
                    Some(Space {
                        id,
                        name,
                        cwd: normalize_cwd(cwd),
                        tabs,
                        active,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let current_space = value
        .get("current_space")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|id| spaces.iter().any(|space| &space.id == id));
    let selected = value
        .get("selected")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let provider = value
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (spaces, current_space, selected, model, provider)
}

fn catalog_model_name(catalog: &Value, id: &str) -> Option<String> {
    let groups = catalog.get("groups")?.as_array()?;
    for group in groups {
        let Some(models) = group.get("models").and_then(|v| v.as_array()) else {
            continue;
        };
        for model in models {
            if model.get("id").and_then(|v| v.as_str()) == Some(id) {
                return model
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|name| name.to_string());
            }
        }
    }
    None
}

fn pretty_model(model: &str) -> String {
    match model {
        "" | "dsh" => "dsh".to_string(),
        "dfmodel" => "DFModel".to_string(),
        "deepseek-v4-pro" => "DeepSeek V4 Pro".to_string(),
        other => other.to_string(),
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn pretty_cwd(cwd: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut path = if !home.is_empty() {
        cwd.replacen(&home, "~", 1)
    } else {
        cwd.to_string()
    };
    if path.chars().count() > 28 {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() > 3 {
            path = format!("{}/…/{}", parts[0], parts[parts.len() - 1]);
        }
    }
    path
}

fn git_summary(cwd: &str) -> Option<String> {
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !branch.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    if branch.is_empty() {
        return None;
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    let mut modified = 0usize;
    let mut untracked = 0usize;
    for line in String::from_utf8_lossy(&status.stdout).lines() {
        if line.starts_with("??") {
            untracked += 1;
        } else if !line.trim().is_empty() {
            modified += 1;
        }
    }
    let mut summary = branch;
    if modified > 0 {
        summary.push_str(&format!(" *{modified}"));
    }
    if untracked > 0 {
        summary.push_str(&format!(" ?{untracked}"));
    }
    Some(summary)
}

fn short_cwd(cwd: &str) -> String {
    if cwd.is_empty() {
        return "(no cwd)".into();
    }
    let parts: Vec<&str> = cwd.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn main() {
    // Ctrl-C 也走 Drop（杀整个 host 进程组）
    ctrlc_proxy();
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        // gpui-component defaults to the system/light palette; the app shell is
        // dark, so force the component theme (Input, TextView, menus) to match.
        gpui_kit::component::theme::Theme::change(
            gpui_kit::component::theme::ThemeMode::Dark,
            None,
            cx,
        );
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(40.), px(40.)),
                size(px(1100.), px(720.)),
            ))),
            // Draw our own dark title row (minimize / maximize / close).
            window_decorations: Some(gpui_kit::WindowDecorations::Client),
            app_id: Some("omd".to_string()),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |_window, cx| {
            cx.new(|cx| Root::new(_window, cx))
        })
        .expect("failed to open window");
    });
}

/// 直接写文件，绕过 stdout 缓冲（worker 线程的 eprintln 不可靠）
fn trace(args: std::fmt::Arguments) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/omd-trace.log")
    {
        let _ = writeln!(f, "{args}");
    }
}


#[cfg(unix)]
fn ctrlc_proxy() {
    // SIGINT/SIGTERM 默认行为即终止进程；kill_on_drop + 进程组 Drop 由 worker 线程持有。
    // Rust 的 Drop 不保证在信号退出时执行，所以额外注册 handler 显式清理。
    unsafe {
        extern "C" fn on_signal(sig: i32) {
            // The host is a user-level daemon now; a GUI signal must not kill it.
            extern "C" {
                fn _exit(code: i32) -> !;
            }
            let _ = sig;
            unsafe { _exit(0) };
        }
        extern "C" {
            fn signal(sig: i32, handler: extern "C" fn(i32)) -> i64;
        }
        signal(2, on_signal); // SIGINT
        signal(15, on_signal); // SIGTERM
    }
}

#[cfg(not(unix))]
fn ctrlc_proxy() {}

/// 按键 → PTY 字节。覆盖 v1 需要的键；未映射返回 None（冒泡）。
fn key_to_bytes(keystroke: &Keystroke) -> Option<String> {
    let key = keystroke.key.as_str();
    let ctrl = keystroke.modifiers.control;
    let shift = keystroke.modifiers.shift;
    // ctrl+字母 → 控制字符
    if ctrl && key.len() == 1 {
        if let Some(c) = key.chars().next() {
            if c.is_ascii_lowercase() {
                return Some(((c as u8 - b'a' + 1) as char).to_string());
            }
        }
    }
    Some(match key {
        "enter" => "\r".into(),
        "backspace" => "\x7f".into(),
        "tab" => "\t".into(),
        "escape" => "\x1b".into(),
        "up" => "\x1b[A".into(),
        "down" => "\x1b[B".into(),
        "right" => "\x1b[C".into(),
        "left" => "\x1b[D".into(),
        "home" => "\x1b[H".into(),
        "end" => "\x1b[F".into(),
        "delete" => "\x1b[3~".into(),
        "pageup" => "\x1b[5~".into(),
        "pagedown" => "\x1b[6~".into(),
        "space" => " ".into(),
        c if c.chars().count() == 1 => {
            let ch = c.chars().next().unwrap();
            if shift && ch.is_ascii_lowercase() {
                ch.to_ascii_uppercase().to_string()
            } else {
                c.to_string()
            }
        }
        _ => return None,
    })
}


/// base64 → BGRA RenderImage（GPUI 要 BGRA）。
fn decode_image(b64: &str) -> Option<RenderImage> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let loaded = image::load_from_memory(&bytes).ok()?;
    let rgba = loaded.to_rgba8();
    let (w, h) = rgba.dimensions();
    // RGBA → BGRA
    let mut bgra = rgba.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(w, h, bgra)?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}
