//! M0 spike：启动 host → 鉴权 → 列会话 → 订阅事件 → 跟随一条会话的事件流。
//!
//! 用法：
//!   cargo run -p dsh-spike                     # 用 web profile
//!   cargo run -p dsh-spike -- --events 5       # 事件订阅秒数
//!   DSH_BIN=~/.npm/_npx/<hash>/node_modules/@deepseek-ai/dsh/lib/bin.js cargo run -p dsh-spike

use std::time::Duration;

use anyhow::{anyhow, Result};
use dsh_host::DshHost;
use dsh_transport::{Frame, Mux, Rpc};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<()> {
    let mut events_secs = 5u64;
    let mut follow_msgs = 3usize;
    let mut list_only = false;
    let mut prompt: Option<String> = None;
    let mut probe: Option<(String, Value)> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--events" => events_secs = args.next().unwrap_or_default().parse().unwrap_or(5),
            "--follow" => follow_msgs = args.next().unwrap_or_default().parse().unwrap_or(3),
            "--list-only" => list_only = true,
            "--prompt" => prompt = Some(args.next().ok_or_else(|| anyhow!("--prompt needs text"))?),
            "--call" => {
                let endpoint = args.next().ok_or_else(|| anyhow!("--call needs an endpoint"))?;
                probe = Some((endpoint, Value::Null));
            }
            "--args" => {
                let text = args.next().ok_or_else(|| anyhow!("--args needs JSON"))?;
                let value: Value = serde_json::from_str(&text)?;
                let Some((_, slot)) = probe.as_mut() else {
                    return Err(anyhow!("--args must follow --call"));
                };
                *slot = value;
            }
            other => return Err(anyhow!("unknown arg: {other}")),
        }
    }

    println!("== launching dsh host ==");
    let host = DshHost::launch("web").await?;
    println!("base: {}", host.base());
    println!("pid: {:?}", host.pid());

    let rpc = Rpc::new(host.client(), host.base());

    println!("\n== session/list ==");
    let list = rpc.call("session/list", json!({"_request": {}})).await?;
    let items = list
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    println!("sessions: {}", items.len());
    for item in items.iter().take(5) {
        let id = item.get("sessionId").and_then(|v| v.as_str()).unwrap_or("?");
        let cwd = item.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
        let running = item.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                item.get("projections")
                    .and_then(|p| p.get("title"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        println!("  - {id}  running={running} cwd={cwd} {title}");
    }

    if std::env::var("DSH_SPIKE_CONTROL").is_ok() {
        // session/control：基线 + projection 增量（GUI 恢复 running 状态用）
        let mux2 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
        let mut st = mux2.open("session/control", json!({})).await?;
        println!("CONTROL open");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut n = 0;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(3), st.recv()).await {
                Ok(Some(Frame::Item(v))) => {
                    n += 1;
                    let text = v.to_string();
                    println!("CONTROL frame[{n}]: {}", text.chars().take(60000).collect::<String>());
                }
                Ok(Some(Frame::End)) => {
                    println!("CONTROL end");
                    break;
                }
                Ok(Some(Frame::Error(e))) => {
                    println!("CONTROL error: {e:?}");
                    break;
                }
                _ => { println!("CONTROL recv timeout"); break; }
            }
        }
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_WATERFALL").is_ok() {
        // 悬着的 approval 能不能在「重连」后补发（GUI 重启恢复的关键）：
        // 1) 建会话（默认 read-only，跑命令要审批）2) events#1 挂着
        // 3) prompt 让它跑 touch → 等 approval 4) 开 events#2（模拟重启）
        // 5) 看 waterfall 是否补发 pending approval 6) 收尾 decide
        let created = rpc.call("session/create", json!({ "request": { "cwd": "/tmp" } })).await?;
        let sid = created.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        println!("WATERFALL session: {sid}");
        let mux1 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
        let mut ev1 = mux1.open("$events", json!({})).await?;
        println!("WATERFALL events#1 open");
        let _ = rpc.call("session/prompt", json!({
            "request": { "requestId": format!("wf-{sid}"), "sessionId": sid, "mode": "queue",
                         "content": [ { "type": "text", "text": "Run this exact shell command and show the output: touch /tmp/omd-wf-probe && echo done" } ] }
        })).await;
        println!("WATERFALL prompt sent");
        let mut pending_id = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(10), ev1.recv()).await {
                Ok(Some(Frame::Item(v))) => {
                    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                    let event = v.get("event").and_then(|t| t.as_str()).unwrap_or("?");
                    println!("WATERFALL ev1 type={kind} event={event}");
                    if event == "api-session/error" || event == "api-session/status" {
                        println!("WATERFALL detail: {}", v.to_string().chars().take(500).collect::<String>());
                    }
                    if event == "approval/request" {
                        pending_id = v.get("eventId").and_then(|t| t.as_str()).unwrap_or("").to_string();
                        println!("WATERFALL pending approval {pending_id}");
                        break;
                    }
                }
                _ => { println!("WATERFALL ev1 ended"); break; }
            }
        }
        if pending_id.is_empty() {
            println!("WATERFALL no approval appeared (agent may be allowed already) — set read-only preset first");
            return Ok(());
        }
        // 模拟 GUI 重启：全新连接、全新 $events
        let mux2 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
        let mut ev2 = mux2.open("$events", json!({})).await?;
        println!("WATERFALL events#2 open (reconnect)");
        let mut replayed = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), ev2.recv()).await {
                Ok(Some(Frame::Item(v))) => {
                    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                    let event = v.get("event").and_then(|t| t.as_str()).unwrap_or("?");
                    let eid = v.get("eventId").and_then(|t| t.as_str()).unwrap_or("");
                    println!("WATERFALL ev2 type={kind} event={event} id={eid}");
                    if event == "approval/request" && eid == pending_id {
                        replayed = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        println!("WATERFALL replayed={replayed}");
        // 收尾：放行（approval 卡消失，agent 跑完）
        let client_id = mux2.client_id().unwrap_or_default();
        let resp = rpc.call("$events/result", json!({
            "clientId": client_id, "eventId": pending_id,
            "outcome": { "kind": "result", "value": "allowed-once" }
        })).await;
        println!("WATERFALL decide: {resp:?}");
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_IDLE").is_ok() {
        let created = rpc.call("session/create", json!({ "request": { "cwd": "/tmp" } })).await?;
        let sid = created.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mux2 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
        let mut st = mux2.open("session/follow", json!({
            "request": { "address": { "kind": "session", "sessionId": sid }, "maxMessages": 200 }
        })).await?;
        // 读完 baseline
        let mut hist = 0;
        while let Ok(Some(Frame::Item(_))) = tokio::time::timeout(std::time::Duration::from_secs(2), st.recv()).await {
            hist += 1;
        }
        println!("IDLE baseline {hist} frames, sleeping 30s...");
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        println!("IDLE awake, sending prompt");
        let _ = rpc.call("session/prompt", json!({
            "request": { "requestId": format!("idle-{sid}"), "sessionId": sid, "mode": "queue",
                         "content": [ { "type": "text", "text": "Say: idle test" } ] }
        })).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
        let mut n = 0;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(10), st.recv()).await {
                Ok(Some(Frame::Item(v))) => {
                    n += 1;
                    println!("IDLE live[{n}] type={}", v.pointer("/event/type").and_then(|x| x.as_str()).unwrap_or(v.get("type").and_then(|t| t.as_str()).unwrap_or("?")));
                    if v.pointer("/event/type").and_then(|x| x.as_str()) == Some("turn/end") { break; }
                }
                Err(_) => { println!("IDLE TIMEOUT after {n}"); break; }
                _ => break,
            }
        }
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_ORDER").is_ok() {
        // 先开 $events，再开 follow，再 prompt —— 模拟 GUI 的顺序
        let mux2 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
        let mut _events = mux2.open("$events", json!({})).await?;
        println!("ORDER events opened");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let created = rpc.call("session/create", json!({ "request": { "cwd": "/tmp" } })).await?;
        let sid = created.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut st = mux2.open("session/follow", json!({
            "request": { "address": { "kind": "session", "sessionId": sid }, "maxMessages": 200 }
        })).await?;
        println!("ORDER follow opened {sid}");
        // 先把历史帧读完
        let mut hist = 0;
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), st.recv()).await {
                Ok(Some(Frame::Item(_))) => { hist += 1; }
                _ => break,
            }
        }
        println!("ORDER history frames: {hist}");
        let _ = rpc.call("session/prompt", json!({
            "request": { "requestId": format!("ord-{sid}"), "sessionId": sid, "mode": "queue",
                         "content": [ { "type": "text", "text": "Say: order test" } ] }
        })).await;
        println!("ORDER prompt sent");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut n = 0;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(8), st.recv()).await {
                Ok(Some(Frame::Item(v))) => {
                    n += 1;
                    let t = v.pointer("/event/type").and_then(|x| x.as_str()).unwrap_or("baseline");
                    println!("ORDER live[{n}] {t}");
                    if t == "turn/end" { break; }
                }
                Err(_) => { println!("ORDER timeout after {n} live frames"); break; }
                _ => break,
            }
        }
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_DUAL").is_ok() {
        // 同时开 $events + session/follow，看 follow 是否还能收到新事件
        let mux2 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
        let mut events = mux2.open("$events", json!({})).await?;
        let created = rpc.call("session/create", json!({ "request": { "cwd": "/tmp" } })).await?;
        let sid = created.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut st = mux2.open("session/follow", json!({
            "request": { "address": { "kind": "session", "sessionId": sid }, "maxMessages": 200 }
        })).await?;
        println!("DUAL opened both streams");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let _ = rpc.call("session/prompt", json!({
            "request": { "requestId": format!("dual-{sid}"), "sessionId": sid, "mode": "queue",
                         "content": [ { "type": "text", "text": "Say: dual test" } ] }
        })).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut n = 0;
        while std::time::Instant::now() < deadline {
            tokio::select! {
                ev = events.recv() => { let _ = ev; }
                fr = st.recv() => {
                    match fr {
                        Some(Frame::Item(v)) => {
                            n += 1;
                            let t = v.pointer("/event/type").and_then(|x| x.as_str()).unwrap_or("baseline");
                            println!("DUAL follow[{n}] {t}");
                            if t == "turn/end" { break; }
                        }
                        _ => break,
                    }
                }
            }
        }
        println!("DUAL done {n} frames");
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_FOLLOW_LIVE").is_ok() {
        let created = rpc.call("session/create", json!({ "request": { "cwd": "/tmp" } })).await?;
        let sid = created.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mux2 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
        let mut st = mux2.open("session/follow", json!({
            "request": { "address": { "kind": "session", "sessionId": sid }, "maxMessages": 200 }
        })).await?;
        println!("LIVE follow opened {sid}");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let accepted = rpc.call("session/prompt", json!({
            "request": { "requestId": format!("live-{sid}"), "sessionId": sid, "mode": "queue",
                         "content": [ { "type": "text", "text": "Say: live test" } ] }
        })).await;
        println!("LIVE prompt: {accepted:?}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        let mut n = 0;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(8), st.recv()).await {
                Ok(Some(Frame::Item(v))) => {
                    n += 1;
                    let t = v.pointer("/event/type").and_then(|x| x.as_str()).unwrap_or("baseline");
                    println!("LIVE frame[{n}] {t}");
                    if t == "turn/end" { break; }
                }
                Ok(Some(Frame::End)) => { println!("LIVE end"); break; }
                Ok(Some(Frame::Error(e))) => { println!("LIVE error {e}"); break; }
                Err(_) => { println!("LIVE timeout after {n} frames"); break; }
                _ => break,
            }
        }
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_IMAGE").is_ok() {
        let b64 = std::fs::read_to_string("/tmp/test8x8.b64").unwrap_or_default();
        let created = rpc.call("session/create", json!({ "request": { "cwd": "/tmp" } })).await?;
        let sid = created.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let accepted = rpc.call("session/prompt", json!({
            "request": {
                "requestId": format!("img-{sid}"),
                "sessionId": sid,
                "mode": "queue",
                "content": [
                    { "type": "image", "mediaType": "image/png", "data": b64 },
                    { "type": "text", "text": "Describe this image in 3 words" }
                ],
            }
        })).await;
        println!("IMAGE prompt: {accepted:?}");
        let mux2 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
        let mut st = mux2.open("session/follow", json!({
            "request": { "address": { "kind": "session", "sessionId": sid }, "assistantStream": true }
        })).await?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(20), st.recv()).await {
                Ok(Some(Frame::Item(v))) => {
                    let txt = v.to_string();
                    if txt.contains("image") || txt.contains("attachment") || txt.contains("assistant") {
                        println!("IMAGE frame: {}", txt.chars().take(700).collect::<String>());
                    }
                    if txt.contains("turn/end") { break; }
                }
                _ => break,
            }
        }
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_TERM").is_ok() {
        let created = rpc.call("session/create", json!({ "request": { "cwd": "/tmp" } })).await?;
        let sid = created.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        println!("PROBE session: {sid}");
        let r = rpc.call("terminal/create", json!({
            "agentId": sid,
            "request": { "id": "probe-term", "cols": 80, "rows": 24 }
        })).await;
        println!("PROBE terminal/create: {r:?}");
        if let Ok(v) = r {
            let _ = v;
            let mux2 = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
            let mut st = mux2.open("terminal/attach", json!({ "agentId": sid, "id": "probe-term" })).await?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let mut n = 0;
            while std::time::Instant::now() < deadline {
                match tokio::time::timeout(std::time::Duration::from_secs(4), st.recv()).await {
                    Ok(Some(Frame::Item(v))) => { n += 1; println!("PROBE frame[{n}]: {}", v.to_string().chars().take(300).collect::<String>()); }
                    Ok(Some(Frame::End)) | Ok(Some(Frame::Error(_))) => break,
                    _ => {}
                }
            }
            println!("PROBE frames: {n}");
        }
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_RESULT_PROBE").is_ok() {
        let r = rpc
            .call("$events/result", json!({
                "clientId": "probe",
                "eventId": "probe",
                "outcome": { "kind": "result", "value": "allowed-once" }
            }))
            .await;
        println!("RESULT_PROBE: {r:?}");
        return Ok(());
    }

    if std::env::var("DSH_SPIKE_PROBE").is_ok() {
        for ep in ["permissionPresets/catalog", "settings/describe", "llm/listProviders"] {
            match rpc.call(ep, json!({})).await {
                Ok(v) => println!("PROBE {ep}: {}", v.to_string().chars().take(800).collect::<String>()),
                Err(e) => println!("PROBE {ep} ERR: {e}"),
            }
        }
        return Ok(());
    }

    if let Some((endpoint, args)) = probe {
        let sid = items
            .first()
            .and_then(|item| item.get("sessionId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let args = if args.is_null() {
            if endpoint.starts_with("commands/") {
                json!({ "agentId": sid })
            } else {
                json!({})
            }
        } else {
            args
        };
        println!("\n== {endpoint} ==");
        match rpc.call(&endpoint, args).await {
            Ok(v) => {
                let text = serde_json::to_string_pretty(&v)?;
                let cap = std::env::var("DSH_SPIKE_RAW")
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(200_000);
                println!("{}", text.chars().take(cap).collect::<String>());
            }
            Err(e) => println!("ERR: {e}"),
        }
        return Ok(());
    }

    if list_only {
        return Ok(());
    }

    println!("\n== $events (订阅 {events_secs}s) ==");
    let mux = Mux::connect(&host.ws_url("api/remote.mux")?, host.cookie_header()).await?;
    let mut events = mux.open("$events", json!({})).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(events_secs);
    let mut count = 0;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
            Ok(Some(Frame::Item(value))) => {
                count += 1;
                if count <= 10 {
                    println!("  event[{}]: {}", count, brief(&value));
                }
            }
            Ok(Some(Frame::End)) => {
                println!("  events ended");
                break;
            }
            Ok(Some(Frame::Error(err))) => {
                println!("  events error: {err}");
                break;
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    println!("events seen: {count}");

    if let Some(text) = prompt {
        return run_prompt(&rpc, &mux, &text).await;
    }

    if let Some(session_id) = items
        .first()
        .and_then(|i| i.get("sessionId"))
        .and_then(|v| v.as_str())
    {
        println!("\n== session/follow {session_id} (前 {follow_msgs} 条) ==");
        let mut follow = mux
            .open(
                "session/follow",
                json!({
                    "request": {
                        "address": { "kind": "session", "sessionId": session_id },
                        "maxMessages": 50,
                    }
                }),
            )
            .await?;
        let mut seen = 0;
        while seen < follow_msgs {
            match tokio::time::timeout(Duration::from_secs(10), follow.recv()).await {
                Ok(Some(Frame::Item(value))) => {
                    seen += 1;
                    println!("  item[{seen}]: {}", brief(&value));
                }
                Ok(Some(Frame::End)) => {
                    println!("  follow ended");
                    break;
                }
                Ok(Some(Frame::Error(err))) => {
                    println!("  follow error: {err}");
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    println!("  (timeout waiting for follow items)");
                    break;
                }
            }
        }
        follow.cancel();
    } else {
        println!("\n(no session to follow)");
    }

    println!("\nOK");
    Ok(())
}

fn brief(value: &Value) -> String {
    let s = value.to_string();
    let clipped: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        format!("{clipped}…")
    } else {
        clipped
    }
}


/// 新建一个临时会话，发一句话，把事件流打到 stdout，直到 turn 结束。
async fn run_prompt(rpc: &Rpc<'_>, mux: &Mux, text: &str) -> Result<()> {
    let cwd = std::env::var("PWD").unwrap_or_else(|_| "/tmp".into());
    let created = rpc
        .call(
            "session/create",
            json!({ "request": { "cwd": cwd, "agentPreset": "standard" } }),
        )
        .await?;
    let session_id = created
        .get("sessionId")
        .and_then(|v| v.as_str())
        .or_else(|| created.get("id").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow!("session/create returned no id: {created}"))?
        .to_string();
    println!("\n== session/create -> {session_id} (cwd {cwd}) ==");

    let mut follow = mux
        .open(
            "session/follow",
            json!({
                "request": {
                    "address": { "kind": "session", "sessionId": session_id },
                    "assistantStream": true,
                }
            }),
        )
        .await?;

    let request_id = uuid_like();
    let accepted = rpc
        .call(
            "session/prompt",
            json!({
                "request": {
                    "requestId": request_id,
                    "sessionId": session_id,
                    "mode": "queue",
                    "content": [ { "type": "text", "text": text } ],
                }
            }),
        )
        .await?;
    println!("prompt accepted: {accepted}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut n = 0usize;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(30), follow.recv()).await {
            Ok(Some(Frame::Item(value))) => {
                n += 1;
                print_event(&value, n);
                if is_turn_end(&value) {
                    println!("(turn finished)");
                    break;
                }
            }
            Ok(Some(Frame::End)) => {
                println!("follow ended");
                break;
            }
            Ok(Some(Frame::Error(err))) => {
                println!("follow error: {err}");
                break;
            }
            Ok(None) => break,
            Err(_) => {
                println!("(idle 30s, stopping)");
                break;
            }
        }
    }
    follow.cancel();
    println!("\nOK ({n} frames)");
    Ok(())
}

fn print_event(value: &Value, n: usize) {
    if value.get("frame").is_some() {
        let t = value.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        println!("[{n}] FRAME type={t} raw={}", brief(value));
        return;
    }
    // 帧形态：{type:"event", event:{seq,time,type,data}} 或 baseline 帧
    let Some(event) = value.get("event") else {
        let keys: Vec<String> = value
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        println!("[{n}] baseline keys={keys:?}");
        return;
    };
    let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    let seq = event.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
    let data = event.get("data").cloned().unwrap_or(Value::Null);
    let text = extract_text(&data);
    if text.is_empty() {
        println!("[{n}] seq={seq} {kind}");
    } else {
        println!("[{n}] seq={seq} {kind}: {text}");
    }
}

fn is_turn_end(value: &Value) -> bool {
    value
        .get("event")
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .map(|t| t == "turn/end")
        .unwrap_or(false)
}

/// 从事件 data 里尽力取出可读文本（assistant/message 的 content 块）。
fn extract_text(data: &Value) -> String {
    let mut out = String::new();
    for key in ["message", "content", "text"] {
        let Some(node) = data.get(key) else { continue };
        match node {
            Value::String(s) => out.push_str(&truncate_text(s)),
            Value::Array(items) => {
                for item in items {
                    if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                        out.push_str(&truncate_text(t));
                    }
                }
            }
            Value::Object(map) => {
                if let Some(t) = map.get("text").and_then(|v| v.as_str()) {
                    out.push_str(&truncate_text(t));
                }
            }
            _ => {}
        }
    }
    out
}

fn truncate_text(s: &str) -> String {
    if s.contains('\n') {
        let first: Vec<&str> = s.lines().take(3).collect();
        let joined = first.join(" ⏎ ");
        format!("{joined}…")
    } else {
        s.to_string()
    }
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("dsh-spike-{nanos:x}")
}
