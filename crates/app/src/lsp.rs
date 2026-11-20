//! 极简 LSP 客户端：stdio + JSON-RPC，按文件扩展名自动挑语言服务器。
//!
//! 模型：
//! - 一个后台线程（[`spawn`] 返回的 `Sender` 就是它）拥有全部 server 进程，
//!   UI 只往里丢 [`Cmd`]，绝不阻塞渲染线程。
//! - 每个 server 一个 stdout 读线程：响应按 id 路由回 pending，诊断走
//!   [`Event`] 通道回 UI。
//! - 只做编辑器要用的最小集合：diagnostics / completion / hover / definition。
//!
//! 语言覆盖面见 [`candidates`]；没装对应二进制就静默跳过（记一行日志）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

/// 一个语言服务器的启动方式。
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// 同名 server 复用同一个进程
    pub key: String,
    pub cmd: String,
    pub args: Vec<String>,
    /// LSP 的 languageId（rust / typescript / python …）
    pub language_id: String,
}

/// 一条诊断（已经转成 0-based 行列 + UTF-16 code unit）。
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub line: u32,
    pub start: u32,
    pub end_line: u32,
    pub end: u32,
    /// 1 = error, 2 = warning, 3 = info, 4 = hint
    pub severity: u8,
    pub message: String,
    pub source: String,
}

/// 后台线程 → UI 的事件。
#[derive(Clone, Debug)]
pub enum Event {
    Diagnostics {
        path: String,
        items: Vec<Diagnostic>,
    },
    /// 某个 server 起来了 / 挂了（面板头上显示用）
    Server {
        key: String,
        state: ServerState,
    },
    /// 跨文件 WorkspaceEdit：path -> 该文件的编辑（UI 侧负责落到 open buffer 或磁盘）
    ApplyWorkspaceEdit {
        edits: Vec<(String, Vec<lsp_types::TextEdit>)>,
    },
    /// textDocument/formatting 的结果（UI 侧应用到 open buffer）
    Formatted {
        path: String,
        edits: Vec<lsp_types::TextEdit>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerState {
    Starting,
    Ready,
    Failed,
}

/// UI → 后台线程的请求。
pub enum Cmd {
    Open {
        path: String,
        text: String,
    },
    Change {
        path: String,
        text: String,
    },
    Close {
        path: String,
    },
    Completion {
        path: String,
        line: u32,
        character: u32,
        reply: async_channel::Sender<Value>,
    },
    Hover {
        path: String,
        line: u32,
        character: u32,
        reply: async_channel::Sender<Value>,
    },
    Definition {
        path: String,
        line: u32,
        character: u32,
        reply: async_channel::Sender<Value>,
    },
    CodeActions {
        path: String,
        start: (u32, u32),
        end: (u32, u32),
        reply: async_channel::Sender<Value>,
    },
    /// workspace/executeCommand（code action 里带 Command 的情况）
    ExecuteCommand {
        path: String,
        command: String,
        arguments: Value,
    },
    /// 保存后通知 server（textDocument/didSave）
    Save {
        path: String,
        text: String,
    },
    /// 整篇语义高亮（textDocument/semanticTokens/full）
    SemanticTokens {
        path: String,
        reply: async_channel::Sender<Value>,
    },
    /// 格式化整篇（textDocument/formatting）；结果走 Event::Formatted
    Formatting {
        path: String,
        tab_size: u32,
        insert_spaces: bool,
    },
    /// 文档大纲（textDocument/documentSymbol）
    DocumentSymbols {
        path: String,
        reply: async_channel::Sender<Value>,
    },
    Shutdown,
}

fn cfg(key: &str, cmd: &str, args: &[&str], language_id: &str) -> ServerConfig {
    ServerConfig {
        key: key.to_string(),
        cmd: cmd.to_string(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        language_id: language_id.to_string(),
    }
}

/// 扩展名 → 候选 server（按顺序取第一个装了的）。
///
/// 可以用 `OMD_LSP_<EXT>` 覆盖，例如
/// `OMD_LSP_PY="/usr/bin/python3 /tmp/lsp.py --stdio"`、
/// `OMD_LSP_RS="$HOME/.cargo/bin/rust-analyzer --log-file /tmp/ra.log"`。
pub fn candidates(ext: &str) -> Vec<ServerConfig> {
    if let Ok(override_cmd) = std::env::var(format!("OMD_LSP_{}", ext.to_uppercase())) {
        let mut parts = override_cmd.split_whitespace();
        if let Some(cmd) = parts.next() {
            let args: Vec<String> = parts.map(|part| part.to_string()).collect();
            return vec![ServerConfig {
                key: format!("custom-{ext}"),
                cmd: expand_home(cmd),
                args,
                language_id: language_id_for(ext).to_string(),
            }];
        }
    }
    let list: Vec<ServerConfig> = match ext {
        "rs" => vec![cfg("rust-analyzer", "rust-analyzer", &[], "rust")],
        "ts" | "mts" | "cts" => vec![cfg(
            "typescript",
            "typescript-language-server",
            &["--stdio"],
            "typescript",
        )],
        "tsx" => vec![cfg(
            "typescript",
            "typescript-language-server",
            &["--stdio"],
            "typescriptreact",
        )],
        "js" | "mjs" | "cjs" => vec![cfg(
            "typescript",
            "typescript-language-server",
            &["--stdio"],
            "javascript",
        )],
        "jsx" => vec![cfg(
            "typescript",
            "typescript-language-server",
            &["--stdio"],
            "javascriptreact",
        )],
        "py" | "pyi" => vec![
            cfg("basedpyright", "basedpyright-langserver", &["--stdio"], "python"),
            cfg("pyright", "pyright-langserver", &["--stdio"], "python"),
            cfg("pylsp", "pylsp", &[], "python"),
        ],
        "go" => vec![cfg("gopls", "gopls", &[], "go")],
        "c" | "h" => vec![cfg("clangd", "clangd", &["--background-index=false"], "c")],
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" => vec![cfg(
            "clangd",
            "clangd",
            &["--background-index=false"],
            "cpp",
        )],
        "lua" => vec![cfg("lua", "lua-language-server", &[], "lua")],
        "toml" => vec![cfg("taplo", "taplo", &["lsp", "stdio"], "toml")],
        "yaml" | "yml" => vec![cfg("yaml", "yaml-language-server", &["--stdio"], "yaml")],
        "json" | "jsonc" => vec![cfg(
            "json",
            "vscode-json-language-server",
            &["--stdio"],
            "json",
        )],
        "sh" | "bash" | "zsh" => vec![cfg("bash", "bash-language-server", &["start"], "shellscript")],
        "md" | "markdown" => vec![cfg("marksman", "marksman", &["server"], "markdown")],
        "zig" => vec![cfg("zls", "zls", &[], "zig")],
        "nix" => vec![cfg("nixd", "nixd", &[], "nix")],
        _ => vec![],
    };
    list
}

fn language_id_for(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" => "cpp",
        "lua" => "lua",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" | "jsonc" => "json",
        "sh" | "bash" | "zsh" => "shellscript",
        "md" | "markdown" => "markdown",
        "zig" => "zig",
        "nix" => "nix",
        _ => "plaintext",
    }
}

/// 从文件往上找工程根（Cargo.toml / package.json / pyproject.toml / go.mod / .git …）。
///
/// rust-analyzer 之类的 server 需要 workspace 根，给错目录会直接 "failed to fetch workspace"。
pub fn workspace_root(path: &str) -> String {
    const MARKERS: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "tsconfig.json",
        "pyproject.toml",
        "setup.py",
        "go.mod",
        "requirements.txt",
        ".git",
        ".hg",
        "Makefile",
        "build.zig",
        "flake.nix",
    ];
    let mut dir = std::path::Path::new(path)
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    loop {
        if MARKERS.iter().any(|marker| dir.join(marker).exists()) {
            return dir.to_string_lossy().to_string();
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }
    std::path::Path::new(path)
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .unwrap_or_else(|| "/".to_string())
}

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
    }
    path.to_string()
}

/// path -> 原始诊断 JSON（`textDocument/codeAction` 的 context 要带真实诊断）
static RAW_DIAGNOSTICS: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);

fn remember_raw_diagnostics(path: &str, items: Value) {
    let mut map = RAW_DIAGNOSTICS.lock().unwrap();
    map.get_or_insert_with(HashMap::new)
        .insert(path.to_string(), items);
}

fn raw_diagnostics_for(path: &str) -> Value {
    let map = RAW_DIAGNOSTICS.lock().unwrap();
    map.as_ref()
        .and_then(|map| map.get(path).cloned())
        .unwrap_or_else(|| Value::Array(Vec::new()))
}

/// path -> 语义高亮 legend（server initialize 时填）
static LEGENDS: Mutex<Option<HashMap<String, lsp_types::SemanticTokensLegend>>> = Mutex::new(None);

fn remember_legend(path: &str, legend: lsp_types::SemanticTokensLegend) {
    let mut legends = LEGENDS.lock().unwrap();
    legends
        .get_or_insert_with(HashMap::new)
        .insert(path.to_string(), legend);
}

fn legend_for(path: &str) -> lsp_types::SemanticTokensLegend {
    let legends = LEGENDS.lock().unwrap();
    legends
        .as_ref()
        .and_then(|map| map.get(path).cloned())
        .unwrap_or(lsp_types::SemanticTokensLegend {
            token_types: Vec::new(),
            token_modifiers: Vec::new(),
        })
}

fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// PATH 里有没有这个可执行文件。
fn in_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(cmd);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

/// 文件对应的 server（装了的第一个）。
pub fn server_for(path: &str) -> Option<ServerConfig> {
    candidates(&ext_of(path))
        .into_iter()
        .find(|config| in_path(&config.cmd))
}

// ---------------------------------------------------------------------------
// 单个 server
// ---------------------------------------------------------------------------

struct Server {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, Sender<Value>>>,
    diag_tx: async_channel::Sender<Event>,
    config: ServerConfig,
    /// path -> version
    versions: Mutex<HashMap<String, i64>>,
    /// initialize 拿到的语义高亮 legend
    legend: Mutex<Option<lsp_types::SemanticTokensLegend>>,
}

impl Server {
    fn start(config: ServerConfig, diag_tx: async_channel::Sender<Event>) -> Option<Arc<Self>> {
        let mut child = match Command::new(&config.cmd)
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                eprintln!("[lsp] spawn {} failed: {err}", config.cmd);
                return None;
            }
        };
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        // stderr 单独抽走，别让它塞满管道卡住 server
        if let Some(stderr) = child.stderr.take() {
            let key = config.key.clone();
            std::thread::Builder::new()
                .name(format!("lsp-err-{key}"))
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        eprintln!("[lsp:{key}] {line}");
                    }
                })
                .ok();
        }
        let key = config.key.clone();
        let server = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            diag_tx,
            config,
            versions: Mutex::new(HashMap::new()),
            legend: Mutex::new(None),
        });
        let reader = server.clone();
        std::thread::Builder::new()
            .name(format!("lsp-read-{key}"))
            .spawn(move || reader.read_loop(stdout))
            .ok()?;
        Some(server)
    }

    fn read_loop(self: Arc<Self>, stdout: impl Read) {
        let mut reader = BufReader::new(stdout);
        loop {
            // 头：Content-Length: N\r\n\r\n
            let mut len = 0usize;
            let mut saw_header = false;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    break;
                }
                saw_header = true;
                if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                    len = rest.trim().parse().unwrap_or(0);
                }
            }
            if !saw_header || len == 0 {
                continue;
            }
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };
            self.dispatch(msg);
        }
    }

    fn dispatch(&self, msg: Value) {
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        // server → client 的请求：必须回，否则 server 卡住
        if let Some(id) = id.clone() {
            if !method.is_empty() {
                let result = match method {
                    "workspace/configuration" => {
                        let count = msg
                            .get("params")
                            .and_then(|p| p.get("items"))
                            .and_then(|i| i.as_array())
                            .map(|items| items.len())
                            .unwrap_or(1);
                        Value::Array(vec![Value::Null; count])
                    }
                    "workspace/workspaceFolders" => Value::Null,
                    "client/registerCapability" | "client/unregisterCapability" => Value::Null,
                    "window/workDoneProgress/create" => Value::Null,
                    "window/showMessageRequest" => Value::Null,
                    _ => Value::Null,
                };
                self.reply(id, result);
                return;
            }
            // 响应
            if let Some(index) = id.as_i64() {
                if let Some(tx) = self.pending.lock().unwrap().remove(&index) {
                    let _ = tx.send(msg);
                }
            }
            return;
        }

        if method == "textDocument/publishDiagnostics" {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let Some(uri) = params.get("uri").and_then(|u| u.as_str()) else {
                return;
            };
            let path = uri_to_path(uri);
            let items = params
                .get("diagnostics")
                .and_then(|d| d.as_array())
                .map(|list| list.iter().filter_map(parse_diagnostic).collect())
                .unwrap_or_default();
            remember_raw_diagnostics(
                &path,
                params
                    .get("diagnostics")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new())),
            );
            let _ = self
                .diag_tx
                .try_send(Event::Diagnostics { path, items });
        }
    }

    fn write_message(&self, value: &Value) {
        let body = value.to_string();
        let mut stdin = self.stdin.lock().unwrap();
        let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = stdin.flush();
    }

    fn reply(&self, id: Value, result: Value) {
        self.write_message(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn notify(&self, method: &str, params: Value) {
        self.write_message(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// 发请求并等响应（带超时）。后台线程里调用，绝不能从 UI 线程调。
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        match rx.recv_timeout(timeout) {
            // 回的是整个 JSON-RPC 信封，只把 result 拿出去
            Ok(value) => Some(value.get("result").cloned().unwrap_or(Value::Null)),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                None
            }
        }
    }

    fn initialize(&self, root: &str) -> bool {
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "omd", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": path_to_uri(root),
            "workspaceFolders": [{ "uri": path_to_uri(root), "name": "workspace" }],
            "capabilities": {
                "textDocument": {
                    "synchronization": { "dynamicRegistration": false },
                    "completion": {
                        "dynamicRegistration": false,
                        "completionItem": {
                            "snippetSupport": false,
                            "documentationFormat": ["markdown", "plaintext"],
                        },
                    },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": false },
                    "codeAction": {
                        "dynamicRegistration": false,
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": ["quickfix", "refactor", "refactor.extract",
                                             "refactor.inline", "refactor.rewrite",
                                             "source", "source.organizeImports", "source.fixAll"]
                            }
                        },
                        "resolveSupport": { "properties": ["edit"] },
                    },
                    "semanticTokens": {
                        "dynamicRegistration": false,
                        "requests": { "range": true, "full": true },
                        "tokenTypes": [],
                        "tokenModifiers": [],
                        "formats": ["relative"],
                        "overlappingTokenSupport": false,
                        "multilineTokenSupport": false,
                    },
                    "publishDiagnostics": {
                        "relatedInformation": false,
                        "versionSupport": true,
                    },
                },
                "workspace": { "workspaceFolders": true, "configuration": true },
                "window": { "workDoneProgress": true, "showMessage": {} },
            },
            "initializationOptions": {},
        });
        let Some(reply) = self.request("initialize", params, Duration::from_secs(30)) else {
            eprintln!("[lsp] {} initialize 超时", self.config.key);
            return false;
        };
        if let Some(error) = reply.get("error") {
            eprintln!("[lsp] {} initialize 失败: {error}", self.config.key);
            return false;
        }
        if let Some(legend) = reply
            .get("capabilities")
            .and_then(|caps| caps.get("semanticTokensProvider"))
            .and_then(|provider| provider.get("legend"))
            .cloned()
        {
            if let Ok(legend) = serde_json::from_value::<lsp_types::SemanticTokensLegend>(legend) {
                *self.legend.lock().unwrap() = Some(legend);
            }
        }
        self.notify("initialized", json!({}));
        true
    }

    fn did_open(&self, path: &str, text: &str) {
        let version = {
            let mut versions = self.versions.lock().unwrap();
            let version = versions.entry(path.to_string()).or_insert(0);
            *version += 1;
            *version
        };
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": path_to_uri(path),
                    "languageId": self.config.language_id,
                    "version": version,
                    "text": text,
                }
            }),
        );
    }

    fn did_change(&self, path: &str, text: &str) {
        let version = {
            let mut versions = self.versions.lock().unwrap();
            let version = versions.entry(path.to_string()).or_insert(0);
            *version += 1;
            *version
        };
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": path_to_uri(path), "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    fn did_save(&self, path: &str, text: &str) {
        let _ = text;
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": path_to_uri(path) } }),
        );
    }

    fn did_close(&self, path: &str) {
        self.versions.lock().unwrap().remove(path);
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": path_to_uri(path) } }),
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

fn parse_diagnostic(item: &Value) -> Option<Diagnostic> {
    let range = item.get("range")?;
    let start = range.get("start")?;
    Some(Diagnostic {
        line: start.get("line")?.as_u64()? as u32,
        start: start.get("character")?.as_u64()? as u32,
        end_line: range
            .get("end")
            .and_then(|e| e.get("line"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0) as u32,
        end: range
            .get("end")
            .and_then(|e| e.get("character"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as u32,
        severity: item.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) as u8,
        message: item
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
        source: item
            .get("source")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

pub fn path_to_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for byte in path.as_bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// 后台线程
// ---------------------------------------------------------------------------

/// 起后台线程，返回 (请求发送端, 事件接收端)。
pub fn spawn() -> (Sender<Cmd>, async_channel::Sender<Event>, async_channel::Receiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (ev_tx, ev_rx) = async_channel::unbounded::<Event>();
    let worker_ev = ev_tx.clone();
    std::thread::Builder::new()
        .name("lsp-worker".to_string())
        .spawn(move || worker(cmd_rx, worker_ev))
        .expect("spawn lsp worker");
    (cmd_tx, ev_tx, ev_rx)
}

/// 没有事件通道的 provider（自检/单元用）。
pub fn noop_event_sender() -> async_channel::Sender<Event> {
    static CHANNEL: Mutex<Option<async_channel::Sender<Event>>> = Mutex::new(None);
    let mut cached = CHANNEL.lock().unwrap();
    if cached.is_none() {
        let (tx, _rx) = async_channel::unbounded();
        *cached = Some(tx);
    }
    cached.as_ref().unwrap().clone()
}

/// 把一组 LSP TextEdit 应用到字符串（按起点倒序，避免偏移互相顶歪）。
pub fn apply_text_edits(text: &str, edits: &[lsp_types::TextEdit]) -> String {
    let mut resolved: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            let start = offset_for(text, edit.range.start.line, edit.range.start.character);
            let end = offset_for(text, edit.range.end.line, edit.range.end.character).max(start);
            (start, end, edit.new_text.as_str())
        })
        .collect();
    resolved.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut out = text.to_string();
    for (start, end, new_text) in resolved {
        if start <= end && end <= out.len() {
            out.replace_range(start..end, new_text);
        }
    }
    out
}

fn worker(cmd_rx: Receiver<Cmd>, ev_tx: async_channel::Sender<Event>) {
    let mut manager = Manager {
        servers: HashMap::new(),
        routed: HashMap::new(),
        ev_tx,
    };
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Cmd::Shutdown => break,
            Cmd::Open { path, text } => manager.sync(path, text, false),
            Cmd::Change { path, text } => manager.sync(path, text, true),
            Cmd::Close { path } => {
                if let Some(key) = manager.routed.remove(&path) {
                    if let Some(server) = manager.servers.get(&key) {
                        server.did_close(&path);
                    }
                }
            }
            Cmd::Completion {
                path,
                line,
                character,
                reply,
            } => manager.query("textDocument/completion", path, line, character, reply),
            Cmd::Hover {
                path,
                line,
                character,
                reply,
            } => manager.query("textDocument/hover", path, line, character, reply),
            Cmd::Definition {
                path,
                line,
                character,
                reply,
            } => manager.query("textDocument/definition", path, line, character, reply),
            Cmd::CodeActions {
                path,
                start,
                end,
                reply,
            } => manager.code_actions(path, start, end, reply),
            Cmd::ExecuteCommand {
                path,
                command,
                arguments,
            } => manager.execute_command(&path, &command, arguments),
            Cmd::SemanticTokens { path, reply } => manager.semantic_tokens(path, reply),
            Cmd::DocumentSymbols { path, reply } => {
                let Some(server) =
                    manager.routed.get(&path).and_then(|key| manager.servers.get(key))
                else {
                    let _ = reply.try_send(Value::Null);
                    return;
                };
                let params = json!({ "textDocument": { "uri": path_to_uri(&path) } });
                let result = server
                    .request("textDocument/documentSymbol", params, Duration::from_secs(8))
                    .unwrap_or(Value::Null);
                let _ = reply.try_send(result);
            }
            Cmd::Formatting {
                path,
                tab_size,
                insert_spaces,
            } => manager.format(&path, tab_size, insert_spaces),
            Cmd::Save { path, text } => {
                if let Some(key) = manager.routed.get(&path) {
                    if let Some(server) = manager.servers.get(key) {
                        server.did_save(&path, &text);
                    }
                }
            }
        }
    }
    for server in manager.servers.values() {
        if let Ok(mut child) = server.child.lock() {
            let _ = child.kill();
        }
    }
}

struct Manager {
    servers: HashMap<String, Arc<Server>>,
    routed: HashMap<String, String>,
    ev_tx: async_channel::Sender<Event>,
}

impl Manager {
    fn server_for(&mut self, path: &str) -> Option<(Arc<Server>, String)> {
        let root = workspace_root(path);
        let config = server_for(path)?;
        if let Some(existing) = self.servers.get(&config.key) {
            return Some((existing.clone(), config.key.clone()));
        }
        let _ = self.ev_tx.try_send(Event::Server {
            key: config.key.to_string(),
            state: ServerState::Starting,
        });
        let started = Server::start(config.clone(), self.ev_tx.clone());
        let state = if started.is_some() {
            ServerState::Ready
        } else {
            ServerState::Failed
        };
        let _ = self.ev_tx.try_send(Event::Server {
            key: config.key.to_string(),
            state,
        });
        let server = started?;
        if !server.initialize(&root) {
            return None;
        }
        self.servers.insert(config.key.clone(), server.clone());
        Some((server, config.key))
    }

    fn sync(&mut self, path: String, text: String, change: bool) {
        let Some((server, key)) = self.server_for(&path) else {
            return;
        };
        if let Some(legend) = server.legend.lock().unwrap().clone() {
            remember_legend(&path, legend);
        }
        let already = self.routed.get(&path) == Some(&key);
        self.routed.insert(path.clone(), key);
        if change || already {
            server.did_change(&path, &text);
        } else {
            server.did_open(&path, &text);
        }
    }

    fn code_actions(
        &mut self,
        path: String,
        start: (u32, u32),
        end: (u32, u32),
        reply: async_channel::Sender<Value>,
    ) {
        let Some(server) = self.routed.get(&path).and_then(|key| self.servers.get(key)) else {
            let _ = reply.try_send(Value::Null);
            return;
        };
        // 带上该文档的真实诊断，server 才会给"修这个错"的 quick fix
        let context_diagnostics: Vec<Value> = raw_diagnostics_for(&path)
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        let Some(range) = item.get("range") else {
                            return false;
                        };
                        let diag_start = range
                            .get("start")
                            .and_then(|p| p.get("line"))
                            .and_then(|l| l.as_u64())
                            .unwrap_or(0) as u32;
                        let diag_end = range
                            .get("end")
                            .and_then(|p| p.get("line"))
                            .and_then(|l| l.as_u64())
                            .unwrap_or(0) as u32;
                        diag_start <= end.0 && diag_end >= start.0
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let params = json!({
            "textDocument": { "uri": path_to_uri(&path) },
            "range": {
                "start": { "line": start.0, "character": start.1 },
                "end": { "line": end.0, "character": end.1 },
            },
            "context": { "diagnostics": context_diagnostics },
        });
        let result = server
            .request("textDocument/codeAction", params, Duration::from_secs(8))
            .unwrap_or(Value::Null);
        let _ = reply.try_send(result);
    }

    fn semantic_tokens(&mut self, path: String, reply: async_channel::Sender<Value>) {
        let Some(server) = self.routed.get(&path).and_then(|key| self.servers.get(key)) else {
            let _ = reply.try_send(Value::Null);
            return;
        };
        let params = json!({ "textDocument": { "uri": path_to_uri(&path) } });
        let result = server
            .request("textDocument/semanticTokens/full", params, Duration::from_secs(10))
            .unwrap_or(Value::Null);
        let _ = reply.try_send(result);
    }

    fn execute_command(&mut self, path: &str, command: &str, arguments: Value) {
        let Some(server) = self.routed.get(path).and_then(|key| self.servers.get(key)) else {
            return;
        };
        let params = json!({ "command": command, "arguments": arguments });
        let _ = server.request(
            "workspace/executeCommand",
            params,
            Duration::from_secs(10),
        );
    }

    /// textDocument/formatting → Event::Formatted（编辑列表；空 = 已是格式或 server 不支持）
    fn format(&mut self, path: &str, tab_size: u32, insert_spaces: bool) {
        let Some(server) = self.routed.get(path).and_then(|key| self.servers.get(key)) else {
            eprintln!("[lsp] formatting 没有路由：{path}");
            return;
        };
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "options": { "tabSize": tab_size, "insertSpaces": insert_spaces },
        });
        let result = server
            .request("textDocument/formatting", params, Duration::from_secs(10))
            .unwrap_or(Value::Null);
        let edits: Vec<lsp_types::TextEdit> = serde_json::from_value(result).unwrap_or_default();
        eprintln!("[lsp] formatting {path}: {} edits", edits.len());
        let _ = self
            .ev_tx
            .try_send(Event::Formatted { path: path.to_string(), edits });
    }

    fn query(
        &mut self,
        method: &str,
        path: String,
        line: u32,
        character: u32,
        reply: async_channel::Sender<Value>,
    ) {
        let Some(server) = self.routed.get(&path).and_then(|key| self.servers.get(key)) else {
            eprintln!("[lsp] query {method} 没有路由：{path}（routed 中有 {} 个）", self.routed.len());
            let _ = reply.try_send(Value::Null);
            return;
        };
        let params = json!({
            "textDocument": { "uri": path_to_uri(&path) },
            "position": { "line": line, "character": character },
        });
        let result = server
            .request(method, params, Duration::from_secs(8))
            .unwrap_or(Value::Null);
        let _ = reply.try_send(result);
    }
}

/// 把 (line, utf16 char) 换成字节偏移。找不到就退回文本末尾。
pub fn offset_for(text: &str, line: u32, character: u32) -> usize {
    let mut current_line = 0u32;
    let mut offset = 0usize;
    let mut units = 0u32;
    for ch in text.chars() {
        if current_line == line {
            if units >= character {
                return offset;
            }
            units += ch.len_utf16() as u32;
            offset += ch.len_utf8();
            continue;
        }
        if ch == '\n' {
            current_line += 1;
        }
        offset += ch.len_utf8();
    }
    text.len()
}

// ---------------------------------------------------------------------------
// gpui-base 的 LSP provider 实现
// ---------------------------------------------------------------------------

use anyhow::Result;
use gpui_kit::{App, Task, Window};
use gpui_kit::component::input::{
    CompletionProvider, DefinitionProvider, HoverProvider,
};
use lsp_types::{
    CompletionContext, CompletionResponse, Hover, Location, LocationLink,
};
use gpui_kit::component::input::Rope;

/// 把编辑器里的三个 provider 都指到后台 LSP 线程上的同一个 server。
#[derive(Clone)]
pub struct Providers {
    tx: Sender<Cmd>,
    path: String,
    /// 跨文件编辑要交回 UI 处理（open buffer 走编辑器，其余走磁盘）
    ev_tx: async_channel::Sender<Event>,
    /// 上一次补全结果里"带 additionalTextEdits"的项：(插入文本, 额外编辑)。
    /// 接受补全后 UI 侧按插入文本匹配，再把 import 之类的编辑补上。
    extras: Arc<Mutex<Vec<(String, Vec<lsp_types::TextEdit>)>>>,
}

impl Providers {
    pub fn new(tx: Sender<Cmd>, path: String) -> Self {
        Self::with_events(tx, path, noop_event_sender())
    }

    pub fn with_events(
        tx: Sender<Cmd>,
        path: String,
        ev_tx: async_channel::Sender<Event>,
    ) -> Self {
        Self {
            tx,
            path,
            ev_tx,
            extras: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 共享的 additionalTextEdits 缓存（UI 侧接受补全后查这里）。
    pub fn extras(&self) -> Arc<Mutex<Vec<(String, Vec<lsp_types::TextEdit>)>>> {
        self.extras.clone()
    }

    /// 发一个请求，返回"等回复"的 Task。
    fn request<F, T>(
        &self,
        text: &Rope,
        offset: usize,
        make: F,
        parse: fn(Option<Value>) -> T,
        cx: &mut App,
    ) -> Task<Result<T>>
    where
        F: FnOnce(u32, u32, async_channel::Sender<Value>) -> Cmd,
        T: Send + 'static,
    {
        let (line, character) = position_for(&text.to_string(), offset);
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        let _ = self.tx.send(make(line, character, reply_tx));
        cx.background_executor().spawn(async move {
            let value = reply_rx.recv().await.ok();
            Ok(parse(value))
        })
    }
}

impl CompletionProvider for Providers {
    fn is_completion_trigger(&self, _offset: usize, text: &str, _cx: &mut App) -> bool {
        text.chars().last().is_some_and(|ch| {
            ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == ':' || ch == '/' || ch == '"'
        })
    }

    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let path = self.path.clone();
        let extras = self.extras.clone();
        let task = self.request(
            text,
            offset,
            |line, character, reply| Cmd::Completion {
                path,
                line,
                character,
                reply,
            },
            parse_completion,
            cx,
        );
        // 记住哪些项带了 additionalTextEdits（自动 import 之类）：
        // 把结果透传出去的同时填缓存
        cx.background_executor().spawn(async move {
            let response = task.await;
            if let Ok(ref response) = response {
                let items: Vec<lsp_types::CompletionItem> = match response {
                    CompletionResponse::Array(items) => items.clone(),
                    CompletionResponse::List(list) => list.items.clone(),
                };
                let mut pending = Vec::new();
                for item in items {
                    let Some(edits) = item.additional_text_edits.clone() else {
                        continue;
                    };
                    if edits.is_empty() {
                        continue;
                    }
                    pending.push((completion_insert_text(&item), edits));
                }
                if !pending.is_empty() {
                    eprintln!("[lsp] {} 个补全项带 additionalTextEdits", pending.len());
                }
                *extras.lock().unwrap() = pending;
            }
            response
        })
    }
}

impl HoverProvider for Providers {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        let path = self.path.clone();
        self.request(
            text,
            offset,
            |line, character, reply| Cmd::Hover {
                path,
                line,
                character,
                reply,
            },
            parse_hover,
            cx,
        )
    }
}

impl DefinitionProvider for Providers {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<LocationLink>>> {
        let path = self.path.clone();
        self.request(
            text,
            offset,
            |line, character, reply| Cmd::Definition {
                path,
                line,
                character,
                reply,
            },
            parse_definitions,
            cx,
        )
    }
}

fn parse_completion(value: Option<Value>) -> CompletionResponse {
    {
        let parsed = {
        let Some(value) = value else {
            return CompletionResponse::Array(Vec::new());
        };
        if value.is_null() {
            return CompletionResponse::Array(Vec::new());
        }
        let parsed = serde_json::from_value::<CompletionResponse>(value.clone());
        let parsed = match parsed {
            Ok(response) => response,
            Err(error) => {
                eprintln!("[lsp] completion 解析失败 {error}: {}", &value.to_string().chars().take(300).collect::<String>());
                CompletionResponse::Array(Vec::new())
            }
        };
        parsed
        };
        let count = match &parsed {
            CompletionResponse::Array(items) => items.len(),
            CompletionResponse::List(list) => list.items.len(),
        };
        eprintln!("[lsp] completion: {count} items");
        parsed
    }
}

fn parse_hover(value: Option<Value>) -> Option<Hover> {
    {
        let hover = {
        let value = value?;
        if value.is_null() {
            return None;
        }
        // 老 server 会直接回 Hover；有的回 [Hover]，都要接受
        if let Ok(hover) = serde_json::from_value::<Hover>(value.clone()) {
            return Some(hover);
        }
        value
            .as_array()
            .and_then(|items| items.first().cloned())
            .and_then(|first| serde_json::from_value::<Hover>(first).ok())
        };
        eprintln!("[lsp] hover: {}", if hover.is_some() { "hit" } else { "none" });
        hover
    }
}

fn parse_definitions(value: Option<Value>) -> Vec<LocationLink> {
    {
        let links = {
        let Some(value) = value else {
            return Vec::new();
        };
        if value.is_null() {
            return Vec::new();
        }
        if let Ok(links) = serde_json::from_value::<Vec<LocationLink>>(value.clone()) {
            return links;
        }
        if let Ok(location) = serde_json::from_value::<Location>(value.clone()) {
            return vec![location_to_link(location)];
        }
        if let Ok(locations) = serde_json::from_value::<Vec<Location>>(value) {
            return locations.into_iter().map(location_to_link).collect();
        }
        Vec::new()
        };
        eprintln!("[lsp] definitions: {} links", links.len());
        links
    }
}

fn location_to_link(location: Location) -> LocationLink {
    LocationLink {
        origin_selection_range: None,
        target_uri: location.uri,
        target_range: location.range,
        target_selection_range: location.range,
    }
}

/// 字节偏移 → (line, UTF-16 character)。LSP 的列是 UTF-16 code unit。
pub fn position_for(text: &str, offset: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut character = 0u32;
    for (index, ch) in text.char_indices() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }
    (line, character)
}

// ---------------------------------------------------------------------------
// code action provider
// ---------------------------------------------------------------------------

use gpui_kit::{Entity, SharedString};
use gpui_kit::component::input::{CodeActionProvider, EditorState};
use lsp_types::CodeAction;

impl Providers {
    /// 把一个 (Command | CodeAction) 数组里的 CodeAction 挑出来。
    fn parse_code_actions(value: Option<Value>) -> Vec<CodeAction> {
        let Some(value) = value else {
            return Vec::new();
        };
        let Some(items) = value.as_array() else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        for item in items {
            if item.get("edit").is_some() || item.get("kind").is_some() {
                match serde_json::from_value::<CodeAction>(item.clone()) {
                    Ok(action) => actions.push(action),
                    Err(error) => eprintln!(
                        "[lsp] code action 解析失败 {error}: {}",
                        &item.to_string().chars().take(200).collect::<String>()
                    ),
                }
                continue;
            }
            // 只有 Command：包成一个没有 edit 的 CodeAction
            if let Some(command) = item.get("command").and_then(|c| c.as_str()) {
                if let Ok(parsed) = serde_json::from_value::<CodeAction>(json!({
                    "title": item.get("title").cloned().unwrap_or(Value::String(command.to_string())),
                    "command": item.clone(),
                })) {
                    actions.push(parsed);
                }
            }
        }
        eprintln!("[lsp] code actions: {} items", actions.len());
        actions
    }
}

impl CodeActionProvider for Providers {
    fn id(&self) -> SharedString {
        SharedString::from("lsp")
    }

    fn code_actions(
        &self,
        state: Entity<EditorState>,
        range: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        let text = state.read(cx).value().to_string();
        request_code_actions(&self.tx, &self.path, &text, range, cx)
    }

    fn perform_code_action(
        &self,
        state: Entity<EditorState>,
        action: CodeAction,
        _push_to_history: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        // 按文件分组（跨文件的 import 之类）
        let mut by_path: HashMap<String, Vec<lsp_types::TextEdit>> = HashMap::new();
        if let Some(edit) = action.edit.as_ref() {
            if let Some(changes) = edit.changes.as_ref() {
                for (uri, edits) in changes {
                    by_path
                        .entry(uri_to_path(uri.as_str()))
                        .or_default()
                        .extend(edits.iter().cloned());
                }
            }
            if let Some(lsp_types::DocumentChanges::Edits(edits)) = edit.document_changes.as_ref() {
                for file_edit in edits {
                    let path = uri_to_path(file_edit.text_document.uri.as_str());
                    let collected = file_edit
                        .edits
                        .iter()
                        .map(|text_edit| match text_edit {
                            lsp_types::OneOf::Left(edit) => edit.clone(),
                            lsp_types::OneOf::Right(annotated) => annotated.text_edit.clone(),
                        })
                        .collect::<Vec<_>>();
                    by_path.entry(path).or_default().extend(collected);
                }
            }
        }

        // 当前文件：直接喂编辑器（有 undo / 光标维护）
        if let Some(edits) = by_path.remove(&self.path) {
            let mut sorted = edits;
            let text = state.read(cx).value().to_string();
            sorted.sort_by_key(|edit| {
                std::cmp::Reverse(offset_for(
                    &text,
                    edit.range.start.line,
                    edit.range.start.character,
                ))
            });
            let count = sorted.len();
            state.update(cx, |state, cx| {
                state.apply_lsp_edits(&sorted, window, cx);
            });
            eprintln!("[lsp] code action 改了当前文件 {count} 处");
        }

        // 其它文件：交回 UI（开着的走编辑器，没开的写磁盘）
        if !by_path.is_empty() {
            let paths: Vec<String> = by_path.keys().cloned().collect();
            eprintln!("[lsp] code action 还要改 {} 个其它文件: {paths:?}", paths.len());
            let _ = self.ev_tx.try_send(Event::ApplyWorkspaceEdit {
                edits: by_path.into_iter().collect(),
            });
        }

        if let Some(command) = action.command.as_ref() {
            let _ = self.tx.send(Cmd::ExecuteCommand {
                path: self.path.clone(),
                command: command.command.clone(),
                arguments: command
                    .arguments
                    .clone()
                    .map(Value::Array)
                    .unwrap_or(Value::Null),
            });
        }
        eprintln!("[lsp] applied code action: {}", action.title);
        Task::ready(Ok(()))
    }
}

/// textDocument/codeAction 的请求逻辑（provider 与自检钩子共用）。
pub fn request_code_actions(
    tx: &Sender<Cmd>,
    path: &str,
    text: &str,
    range: std::ops::Range<usize>,
    cx: &mut App,
) -> Task<Result<Vec<CodeAction>>> {
    let start = position_for(text, range.start);
    let end = position_for(text, range.end);
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    let _ = tx.send(Cmd::CodeActions {
        path: path.to_string(),
        start,
        end,
        reply: reply_tx,
    });
    cx.background_executor().spawn(async move {
        let value = reply_rx.recv().await.ok();
        Ok(Providers::parse_code_actions(value))
    })
}

/// 大纲里的一条符号（扁平化后的 DocumentSymbol / SymbolInformation）。
#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub name: String,
    pub kind: String,
    /// 起始行列（0-based）
    pub line: u32,
    pub character: u32,
    pub depth: usize,
}

/// textDocument/documentSymbol → 扁平大纲（层级符号 DFS 展开）。
pub async fn document_symbols(tx: &Sender<Cmd>, path: &str) -> Result<Vec<OutlineItem>> {
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    let _ = tx.send(Cmd::DocumentSymbols {
        path: path.to_string(),
        reply: reply_tx,
    });
    let value = reply_rx.recv().await.map_err(|err| anyhow::anyhow!("{err}"))?;
    if value.is_null() {
        return Ok(Vec::new());
    }
    // 两种形态：DocumentSymbol[]（层级）或 SymbolInformation[]（扁平）
    let mut out = Vec::new();
    if let Ok(symbols) = serde_json::from_value::<Vec<lsp_types::DocumentSymbol>>(value.clone()) {
        fn walk(symbols: &[lsp_types::DocumentSymbol], depth: usize, out: &mut Vec<OutlineItem>) {
            for symbol in symbols {
                let mut range = symbol.selection_range.clone();
                if range.start == range.end {
                    range = symbol.range.clone();
                }
                out.push(OutlineItem {
                    name: symbol.name.clone(),
                    kind: format!("{:?}", symbol.kind).to_lowercase(),
                    line: range.start.line,
                    character: range.start.character,
                    depth,
                });
                if let Some(children) = &symbol.children {
                    walk(children, depth + 1, out);
                }
            }
        }
        walk(&symbols, 0, &mut out);
    } else if let Ok(items) =
        serde_json::from_value::<Vec<lsp_types::SymbolInformation>>(value.clone())
    {
        for item in items {
            out.push(OutlineItem {
                name: item.name.clone(),
                kind: format!("{:?}", item.kind).to_lowercase(),
                line: item.location.range.start.line,
                character: item.location.range.start.character,
                depth: 0,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 语义高亮 provider
// ---------------------------------------------------------------------------

use gpui_kit::component::input::DocumentRangeSemanticTokensProvider;
use lsp_types::{SemanticTokens, SemanticTokensLegend};

impl DocumentRangeSemanticTokensProvider for Providers {
    fn legend(&self) -> SemanticTokensLegend {
        legend_for(&self.path)
    }

    fn semantic_tokens(
        &self,
        _text: &Rope,
        _range: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<SemanticTokens>> {
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        let _ = self.tx.send(Cmd::SemanticTokens {
            path: self.path.clone(),
            reply: reply_tx,
        });
        cx.background_executor().spawn(async move {
            let value = reply_rx.recv().await.ok();
            let tokens = match value {
                Some(value) if !value.is_null() => {
                    serde_json::from_value::<SemanticTokens>(value).unwrap_or(SemanticTokens {
                        result_id: None,
                        data: Vec::new(),
                    })
                }
                _ => SemanticTokens {
                    result_id: None,
                    data: Vec::new(),
                },
            };
            eprintln!("[lsp] semantic tokens: {}", tokens.data.len());
            Ok(tokens)
        })
    }
}

/// 接受补全项时真正插进文档的文本（snippet 关掉了，所以是纯文本）。
fn completion_insert_text(item: &lsp_types::CompletionItem) -> String {
    if let Some(edit) = item.text_edit.as_ref() {
        return match edit {
            lsp_types::CompletionTextEdit::Edit(edit) => edit.new_text.clone(),
            lsp_types::CompletionTextEdit::InsertAndReplace(edit) => edit.new_text.clone(),
        };
    }
    item.insert_text
        .clone()
        .unwrap_or_else(|| item.label.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 rust-analyzer 的 textDocument/formatting 冒烟：
    /// 打开一段没格式化的 Rust，格式化后应收到非空 edits，应用后带换行/空格。
    #[test]
    fn formatting_returns_edits_from_rust_analyzer() {
        if which(&"rust-analyzer".to_string()).is_none() {
            eprintln!("[test] rust-analyzer 不在 PATH，跳过");
            return;
        }
        let (tx, _ev_tx, rx) = spawn();
        let dir = std::env::temp_dir().join("omd-lsp-fmt-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fmt_test.rs");
        std::fs::write(&path, "fn main(){let x=1;println!(\"{}\",x);}\n").unwrap();
        let _ = tx.send(Cmd::Open {
            path: path.to_string_lossy().to_string(),
            text: "fn main(){let x=1;println!(\"{}\",x);}\n".to_string(),
        });
        let _ = tx.send(Cmd::Formatting {
            path: path.to_string_lossy().to_string(),
            tab_size: 4,
            insert_spaces: true,
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while let Ok(event) = rx.recv_blocking() {
            match event {
                Event::Formatted { path, edits } => {
                    assert!(!edits.is_empty(), "rust-analyzer 应返回格式化编辑");
                    let text = "fn main(){let x=1;println!(\"{}\",x);}\n";
                    let formatted = apply_text_edits(text, &edits);
                    eprintln!("[test] formatted: {formatted}");
                    assert!(formatted.contains("fn main() {"), "{formatted}");
                    assert_ne!(formatted, text, "必须发生变化");
                    let _ = tx.send(Cmd::Shutdown);
                    let _ = std::fs::remove_file(&path);
                    return;
                }
                _ => {}
            }
            if std::time::Instant::now() > deadline {
                panic!("等 Event::Formatted 超时");
            }
        }
        panic!("事件流提前关闭");
    }

    fn which(bin: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    }
}

#[cfg(test)]
mod outline_tests {
    use super::*;

    /// 真实 rust-analyzer 的 documentSymbol 冒烟：应返回 main 等符号。
    #[test]
    fn document_symbols_from_rust_analyzer() {
        if !std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path).any(|dir| dir.join("rust-analyzer").is_file())
            })
            .unwrap_or(false)
        {
            eprintln!("[test] rust-analyzer 不在 PATH，跳过");
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, _ev_tx, _rx) = spawn();
            let dir = std::env::temp_dir().join("omd-lsp-outline-test");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("outline_test.rs");
            std::fs::write(&path, "struct Foo { x: u32 }\nfn main() { let f = Foo { x: 1 }; }\n").unwrap();
            let _ = tx.send(Cmd::Open {
                path: path.to_string_lossy().to_string(),
                text: "struct Foo { x: u32 }\nfn main() { let f = Foo { x: 1 }; }\n".to_string(),
            });
            let items = document_symbols(&tx, &path.to_string_lossy()).await.unwrap();
            let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
            eprintln!("[test] outline: {names:?}");
            assert!(names.contains(&"Foo"), "{names:?}");
            assert!(names.contains(&"main"), "{names:?}");
            let main = items.iter().find(|item| item.name == "main").unwrap();
            assert_eq!(main.line, 1, "main 在第 2 行（0-based 1）");
            let _ = tx.send(Cmd::Shutdown);
            let _ = std::fs::remove_file(&path);
        });
    }
}
