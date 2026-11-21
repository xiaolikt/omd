//! 启动并持有 `dsh --profile web` 子进程，完成 launch-token → cookie 鉴权。
//!
//! 协议事实（来自 `@deepseek-ai/dsh-client-connection` 的 browser-auth 实现）：
//! - `dsh web` 在 stdout 打印 `dsh web: http://127.0.0.1:<port>/?token=<launchToken>`
//! - 请求该 URL（带 token）即换取签名 cookie（名为 `dsh-auth-<hash>`），并重定向到不带 token 的根
//! - 之后所有 `/api` HTTP/WS 请求带该 cookie 即通过

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use reqwest::cookie::CookieStore;
use url::Url;

const LAUNCH_PREFIX: &str = "dsh web:";

/// A connected view of the user-level dsh host daemon.
///
/// The daemon is intentionally *not* owned by this process: dropping this
/// handle only drops the HTTP cookie jar, never the Node host. That lets the
/// GUI restart (or a second window open) without paying the host boot cost
/// again, while the full `web` profile keeps every host plugin available.
pub struct DshHost {
    pid: u32,
    base: Url,
    version: String,
    bundled: bool,
    /// Tokenized launch URL, retained so a `/connect` peer cookie can be
    /// re-minted after a WebSocket loss (v2 cookies die with their operator).
    tokenized: Url,
    client: reqwest::Client,
    jar: Arc<reqwest::cookie::Jar>,
}

#[derive(Clone)]
struct DaemonState {
    pid: u32,
    tokenized: String,
    version: String,
    profile: String,
    fingerprint: u64,
}

struct DaemonLock {
    path: PathBuf,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl DshHost {
    /// Attach to the running daemon, or start one if it is missing/stale.
    ///
    /// Preference order for the executable stays the same:
    /// 1) App-managed runtime, 2) `DSH_BIN`, 3) system `dsh`.
    pub async fn launch(profile: &str) -> Result<Self> {
        let started = Instant::now();
        let runtime = DshRuntime::current();
        let (version, bundled) = match &runtime {
            Some(runtime) => (runtime.version.clone(), true),
            None => (
                std::env::var("DSH_BIN")
                    .ok()
                    .and_then(|bin| version_from_bin(Path::new(&bin)))
                    .unwrap_or_else(|| "dev".into()),
                false,
            ),
        };
        let fingerprint = profile_fingerprint(profile);
        let state_path = daemon_state_path(profile);
        let log_path = daemon_log_path(profile);
        let lock_path = daemon_lock_path(profile);

        eprintln!("[host] daemon state: {}", state_path.display());

        // Fast path: another window/app already started the daemon.
        if let Some(host) = Self::attach_existing(
            profile,
            &version,
            fingerprint,
            &state_path,
            Duration::from_secs(3),
        )
        .await?
        {
            eprintln!(
                "[host] attached to daemon pid={} at {} ({}ms)",
                host.pid,
                host.base,
                started.elapsed().as_millis()
            );
            return Ok(host);
        }

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut seen_state = state_mtime(&state_path);
        loop {
            // If another process owns the lock and published a fresh state,
            // attach without re-probing a hung daemon every iteration.
            let current_state = state_mtime(&state_path);
            if current_state != seen_state {
                seen_state = current_state;
                if let Some(host) = Self::attach_existing(
                    profile,
                    &version,
                    fingerprint,
                    &state_path,
                    Duration::from_secs(3),
                )
                .await?
                {
                    eprintln!(
                        "[host] attached to daemon pid={} at {} ({}ms)",
                        host.pid,
                        host.base,
                        started.elapsed().as_millis()
                    );
                    return Ok(host);
                }
            }
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "timed out waiting for dsh host daemon (see {})",
                    log_path.display()
                ));
            }

            if let Some(lock) = DaemonLock::acquire(&lock_path) {
                // Re-check only if another process published state while we
                // were waiting for the lock; otherwise the initial short probe
                // already proved this state unreachable.
                let current_state = state_mtime(&state_path);
                if current_state != seen_state {
                    seen_state = current_state;
                    if let Some(host) = Self::attach_existing(
                        profile,
                        &version,
                        fingerprint,
                        &state_path,
                        Duration::from_secs(3),
                    )
                    .await?
                    {
                        drop(lock);
                        eprintln!(
                            "[host] attached to daemon pid={} at {} ({}ms)",
                            host.pid,
                            host.base,
                            started.elapsed().as_millis()
                        );
                        return Ok(host);
                    }
                }

                // Stale daemon from an older runtime or an edited profile.
                if let Some(old) = read_daemon_state(&state_path) {
                    eprintln!(
                        "[host] replacing daemon pid={} version={} (want version={})",
                        old.pid, old.version, version
                    );
                    stop_daemon(old.pid).await;
                    let _ = fs::remove_file(&state_path);
                }

                let pid = spawn_daemon(runtime.as_ref(), profile, &log_path)?;
                let tokenized = match wait_for_launch_url(&log_path, pid, deadline).await {
                    Ok(url) => url,
                    Err(err) => {
                        stop_daemon(pid).await;
                        return Err(err);
                    }
                };
                let state = DaemonState {
                    pid,
                    tokenized: tokenized.clone(),
                    version: version.clone(),
                    profile: profile.to_string(),
                    fingerprint,
                };
                write_daemon_state(&state_path, &state)?;
                drop(lock);

                let host = Self::from_tokenized(
                    tokenized,
                    pid,
                    version,
                    bundled,
                    Duration::from_secs(15),
                )
                .await?;
                eprintln!(
                    "[host] daemon pid={} ready at {} (cold {}ms)",
                    host.pid,
                    host.base,
                    started.elapsed().as_millis()
                );
                return Ok(host);
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn attach_existing(
        profile: &str,
        version: &str,
        fingerprint: u64,
        state_path: &Path,
        auth_timeout: Duration,
    ) -> Result<Option<Self>> {
        let Some(state) = read_daemon_state(state_path) else {
            return Ok(None);
        };
        if state.profile != profile
            || state.version != version
            || state.fingerprint != fingerprint
            || !pid_alive(state.pid)
        {
            return Ok(None);
        }
        let Ok(tokenized) = Url::parse(&state.tokenized) else {
            return Ok(None);
        };
        match Self::from_tokenized(
            tokenized.to_string(),
            state.pid,
            state.version,
            true,
            auth_timeout,
        )
        .await
        {
            Ok(host) => Ok(Some(host)),
            Err(err) => {
                // A live pid with a dead HTTP surface should not wedge every
                // future launch behind the same stale state file.
                eprintln!("[host] daemon pid={} unreachable: {err}", state.pid);
                Ok(None)
            }
        }
    }

    async fn from_tokenized(
        tokenized: String,
        pid: u32,
        version: String,
        bundled: bool,
        auth_timeout: Duration,
    ) -> Result<Self> {
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let client = reqwest::Client::builder()
            .cookie_provider(jar.clone())
            // /connect uses a 302 to the WebSocket endpoint; following it
            // hangs on the upgrade, so read only the Set-Cookie response.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;

        let tokenized_url = Url::parse(&tokenized).with_context(|| format!("bad url: {tokenized}"))?;
        let base = Self::exchange_token(&client, tokenized_url.clone(), auth_timeout).await?;
        if jar.cookies(&base).is_none() {
            return Err(anyhow!("no session cookie after token exchange"));
        }
        Ok(Self {
            pid,
            base,
            version,
            bundled,
            tokenized: tokenized_url,
            client,
            jar,
        })
    }

    /// 用一次性 launch token 换签名 cookie，返回去掉 token 的 base URL。
    ///
    /// 走 `/connect?token=…&interactions=answer`：只有声明 answer 的 peer 才会收到
    /// 需要回答的 forwarded event（审批 `approval/request`、提问 `user-questions/request`）。
    /// 走 `/?token=…` 得到的是浏览器 peer（interactions=none），永远收不到审批。
    async fn exchange_token(
        client: &reqwest::Client,
        tokenized: Url,
        timeout: Duration,
    ) -> Result<Url> {
        let mut connect = tokenized.clone();
        connect.set_path("/connect");
        let existing: Vec<(String, String)> = tokenized
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        {
            let mut pairs = connect.query_pairs_mut();
            pairs.clear();
            for (k, v) in &existing {
                pairs.append_pair(k, v);
            }
            pairs.append_pair("interactions", "answer");
        }
        let resp = client
            .get(connect)
            .timeout(timeout)
            .send()
            .await
            .context("token exchange request failed")?;
        let status = resp.status();
        // 302 = 成功（cookie 已下发）；200 也接受。
        if !(status.is_success() || status.is_redirection()) {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "connect returned {status}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }
        let mut base = tokenized.clone();
        base.set_query(None);
        base.set_path("/");
        Ok(base)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn bundled(&self) -> bool {
        self.bundled
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    /// Re-mints a `/connect` cookie using a cloned client + shared jar.
    pub async fn auth_cookie(
        client: &reqwest::Client,
        jar: &reqwest::cookie::Jar,
        tokenized: &Url,
    ) -> Result<String> {
        let base = DshHost::exchange_token(
            client,
            tokenized.clone(),
            Duration::from_secs(15),
        )
        .await?;
        jar.cookies(&base)
            .and_then(|header| header.to_str().ok().map(|s| s.to_string()))
            .ok_or_else(|| anyhow!("no cookie after /connect re-auth"))
    }

    /// Tokenized launch URL, for re-minting `/connect` cookies.
    pub fn tokenized_url(&self) -> &Url {
        &self.tokenized
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// 供 WebSocket 握手使用的 `Cookie` 头内容。
    pub fn cookie_header(&self) -> Option<String> {
        let url = self.base.clone();
        let header = self.jar.cookies(&url)?;
        header.to_str().ok().map(|s| s.to_string())
    }

    pub fn http_url(&self, path: &str) -> Result<Url> {
        Ok(self.base.join(path.trim_start_matches('/'))?)
    }

    pub fn ws_url(&self, path: &str) -> Result<String> {
        let mut url = self.base.join(path.trim_start_matches('/'))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow!("cannot set ws scheme"))?;
        Ok(url.to_string())
    }

    pub fn jar(&self) -> Arc<reqwest::cookie::Jar> {
        self.jar.clone()
    }

    pub fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }
}

impl DaemonLock {
    fn acquire(path: &Path) -> Option<Self> {
        if let Ok(meta) = fs::metadata(path) {
            let stale = meta
                .modified()
                .ok()
                .and_then(|time| time.elapsed().ok())
                .map(|age| age > Duration::from_secs(120))
                .unwrap_or(false);
            let dead_owner = fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .map(|pid| !pid_alive(pid))
                .unwrap_or(false);
            if stale || dead_owner {
                let _ = fs::remove_file(path);
            }
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .ok()?;
        let _ = writeln!(file, "{}", std::process::id());
        Some(Self {
            path: path.to_path_buf(),
        })
    }
}

fn spawn_daemon(runtime: Option<&DshRuntime>, profile: &str, log_path: &Path) -> Result<u32> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let _ = writeln!(
        log,
        "\n--- omd host start {} profile={profile} ---",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );

    let mut cmd = if let Some(runtime) = runtime {
        let mut c = Command::new(&runtime.node);
        c.arg(&runtime.bin);
        // Profiles must not pin @deepseek-ai/dsh-*: resolve core packages
        // from the runtime version that launched this host, so updating the
        // runtime carries every plugin with it.
        let mut paths = vec![runtime.dir.join("node_modules").display().to_string()];
        if let Ok(existing) = std::env::var("NODE_PATH") {
            if !existing.is_empty() {
                paths.push(existing);
            }
        }
        let joined = std::env::join_paths(&paths)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|_| paths.join(":"));
        c.env("NODE_PATH", joined);
        c
    } else if let Ok(bin) = std::env::var("DSH_BIN") {
        let mut c = Command::new(exe_name("node"));
        c.arg(&bin);
        c
    } else if cfg!(windows) {
        // `dsh` on Windows is a .cmd shim; CreateProcess only appends .exe.
        Command::new("dsh.cmd")
    } else {
        Command::new("dsh")
    };

    cmd.current_dir(home_dir());
    cmd.arg("--profile")
        .arg(profile)
        .arg("--no-open")
        .arg("--port")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));

    // Detach from the GUI's session, so closing the app (or its terminal)
    // does not SIGHUP the host's Node/pnpm children.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    let child = cmd.spawn().with_context(|| {
        if runtime.is_none() && std::env::var("DSH_BIN").is_err() {
            format!(
                "failed to spawn dsh host daemon: no engine under {} and no `dsh` on PATH \
                 (unset DSH_GUI_NO_AUTO_INSTALL to let the app install its bundled runtime, \
                 or point DSH_BIN at a dsh entrypoint)",
                runtime_root().display()
            )
        } else {
            format!(
                "failed to spawn dsh host daemon ({})",
                cmd.get_program().to_string_lossy()
            )
        }
    })?;
    let pid = child.id();
    // The Child handle is deliberately dropped: kill_on_drop is false and the
    // daemon must outlive this process.
    drop(child);
    Ok(pid)
}

async fn wait_for_launch_url(log_path: &Path, pid: u32, deadline: Instant) -> Result<String> {
    loop {
        if let Ok(text) = fs::read_to_string(log_path) {
            if let Some(line) = text.lines().rev().find(|line| line.contains(LAUNCH_PREFIX)) {
                if let Some((_, rest)) = line.split_once(LAUNCH_PREFIX) {
                    let url = rest.trim();
                    if !url.is_empty() {
                        return Ok(url.to_string());
                    }
                }
            }
        }
        if !pid_alive(pid) {
            let tail = fs::read_to_string(log_path).unwrap_or_default();
            return Err(anyhow!(
                "dsh host daemon exited before printing its url\n{}",
                tail.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>()
            ));
        }
        if Instant::now() > deadline {
            let tail = fs::read_to_string(log_path).unwrap_or_default();
            return Err(anyhow!(
                "timed out waiting for dsh host url (see {})\n{}",
                log_path.display(),
                tail.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>()
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn stop_daemon(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), 15);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .output();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), 9);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(windows)]
    {
        let Ok(output) = Command::new("tasklist")
            .args([
                "/FI",
                &format!("PID eq {pid}"),
                "/NH",
                "/FO",
                "CSV",
            ])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn daemon_state_path(profile: &str) -> PathBuf {
    data_root().join(format!("host-{}.json", profile_slug(profile)))
}

fn daemon_log_path(profile: &str) -> PathBuf {
    data_root().join(format!("host-{}.log", profile_slug(profile)))
}

fn daemon_lock_path(profile: &str) -> PathBuf {
    data_root().join(format!("host-{}.lock", profile_slug(profile)))
}

fn profile_slug(profile: &str) -> String {
    profile
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn state_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|meta| meta.modified().ok())
}

fn read_daemon_state(path: &Path) -> Option<DaemonState> {
    let text = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(DaemonState {
        pid: value.get("pid")?.as_u64()? as u32,
        tokenized: value.get("tokenized")?.as_str()?.to_string(),
        version: value.get("version")?.as_str()?.to_string(),
        profile: value.get("profile")?.as_str()?.to_string(),
        fingerprint: value.get("fingerprint")?.as_u64()?,
    })
}

fn write_daemon_state(path: &Path, state: &DaemonState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let value = serde_json::json!({
        "pid": state.pid,
        "tokenized": state.tokenized,
        "version": state.version,
        "profile": state.profile,
        "fingerprint": state.fingerprint,
    });
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&value)?)?;
    #[cfg(unix)]
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn profile_fingerprint(profile: &str) -> u64 {
    let dir = dsh_home().join("profiles").join(profile);
    let mut hasher = DefaultHasher::new();
    for name in ["package.json", "cordis.yml", "cordis.patch.yml", "pnpm-lock.yaml"] {
        if let Ok(bytes) = fs::read(dir.join(name)) {
            name.hash(&mut hasher);
            bytes.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// The user's home directory, with the Windows variable as a fallback.
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn dsh_home() -> PathBuf {
    if let Ok(home) = std::env::var("DSH_HOME") {
        return PathBuf::from(home);
    }
    home_dir().join(".dsh")
}

/// Executable file name for the current platform.
fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

// ---------------------------------------------------------------------------
// App-managed dsh runtime
// ---------------------------------------------------------------------------

pub struct DshRuntime {
    pub version: String,
    pub dir: PathBuf,
    pub node: PathBuf,
    pub bin: PathBuf,
}

pub fn data_root() -> PathBuf {
    if let Ok(dir) = std::env::var("OMD_DATA_DIR")
        .or_else(|_| std::env::var("DSH_GUI_DATA_DIR"))
    {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join("AppData/Local"));
    #[cfg(not(windows))]
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".local/share"));
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

pub fn runtime_root() -> PathBuf {
    data_root().join("runtime")
}

impl DshRuntime {
    /// The runtime selected by `runtime/current`, if it is complete.
    pub fn current() -> Option<Self> {
        let root = runtime_root();
        let selected = std::fs::read_to_string(root.join("current")).ok()?;
        let selected = selected.trim();
        if selected.is_empty() {
            return None;
        }
        let dir = if Path::new(selected).is_absolute() {
            PathBuf::from(selected)
        } else {
            root.join(selected)
        };
        let bin = dir.join("node_modules/@deepseek-ai/dsh/lib/bin.js");
        if !bin.is_file() {
            return None;
        }
        let version = version_from_bin(&bin).unwrap_or_else(|| selected.to_string());
        let _ = write_metadata(&root, &version);
        let bundled = root.join(exe_name("node"));
        let node = if bundled.is_file() {
            bundled
        } else {
            PathBuf::from(exe_name("node"))
        };
        Some(Self {
            version,
            dir,
            node,
            bin,
        })
    }
}

fn version_from_bin(bin: &Path) -> Option<String> {
    // …/@deepseek-ai/dsh/lib/bin.js → …/@deepseek-ai/dsh/package.json
    let pkg = bin.parent()?.parent()?.join("package.json");
    let text = std::fs::read_to_string(pkg).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("version")?.as_str().map(|s| s.to_string())
}

fn node_in_path_named(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn node_in_path() -> Option<PathBuf> {
    node_in_path_named(&exe_name("node"))
}

fn copy_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &to)?;
            #[cfg(windows)]
            {
                // Windows symlinks need developer mode or admin rights; copying
                // the target is enough for the bundled npm tree.
                let target = std::fs::canonicalize(entry.path())?;
                if target.is_dir() {
                    copy_dir(&target, &to)?;
                } else {
                    std::fs::copy(&target, &to)?;
                }
            }
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Copies the npm CLI into the app directory so updates do not depend on a
/// globally installed npm (the Node runtime is already copied by `ensure_node`).
fn ensure_npm(root: &Path) -> Option<PathBuf> {
    let cli = root.join("npm/bin/npm-cli.js");
    if cli.is_file() {
        return Some(cli);
    }
    // Unix: `npm` in PATH resolves to .../node_modules/npm/bin/npm-cli.js.
    // Windows ships `npm.cmd` instead, so derive the CLI from node.exe.
    let real = node_in_path_named("npm")
        .and_then(|npm| std::fs::canonicalize(npm).ok())
        .filter(|real| real.file_name().and_then(|n| n.to_str()) == Some("npm-cli.js"))
        .or_else(|| {
            let node = node_in_path()?;
            let cli = node.parent()?.join("node_modules/npm/bin/npm-cli.js");
            cli.is_file().then_some(cli)
        })?;
    let package = real.parent()?.parent()?;
    copy_dir(package, &root.join("npm")).ok()?;
    Some(cli)
}

fn ensure_node(root: &Path) -> anyhow::Result<PathBuf> {
    let target = root.join(exe_name("node"));
    if target.is_file() {
        return Ok(target);
    }
    let source = node_in_path()
        .ok_or_else(|| anyhow!("node not found in PATH; cannot install the built-in runtime"))?;
    std::fs::copy(&source, &target)
        .with_context(|| format!("copy node from {} to {}", source.display(), target.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&target)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target, perms)?;
    }
    Ok(target)
}

/// Tern-style manager metadata: `manager.json`, `state.json` and a stable
/// `bin/dsh` launcher, all inside the app-managed directory.
fn write_metadata(root: &Path, version: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(root.join("bin"))?;
    std::fs::write(
        root.join("manager.json"),
        "{\"manager\":\"omd\",\"name\":\"OMD\"}\n",
    )?;
    #[cfg(unix)]
    let launcher = root.join("bin/dsh");
    #[cfg(unix)]
    let script = format!(
        "#!/bin/sh\nROOT=$(CDPATH= cd -- \"$(dirname -- \"$0\")/..\" && pwd)\nVER=$(cat \"$ROOT/current\")\nexec \"$ROOT/node\" \"$ROOT/$VER/node_modules/@deepseek-ai/dsh/lib/bin.js\" \"$@\"\n"
    );
    #[cfg(windows)]
    let launcher = root.join("bin/dsh.cmd");
    #[cfg(windows)]
    let script = "@echo off\r\nsetlocal\r\nset ROOT=%~dp0..\r\nset /p VER=<\"%ROOT%\\current\"\r\n\"%ROOT%\\node.exe\" \"%ROOT%\\%VER%\\node_modules\\@deepseek-ai\\dsh\\lib\\bin.js\" %*\r\n";
    let state = serde_json::json!({
        "managed": true,
        "installed": {
            "version": version,
            "via": { "how": "npm" }
        },
        "link": launcher.display().to_string(),
    });
    std::fs::write(
        root.join("state.json"),
        serde_json::to_vec_pretty(&state)?,
    )?;

    std::fs::write(&launcher, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&launcher)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&launcher, perms)?;
    }
    Ok(())
}

/// Installs `@deepseek-ai/dsh@tag` into a new versioned directory and points
/// `runtime/current` at it. The running host (if any) keeps using the old
/// directory; the new one takes effect on the next app launch.
pub async fn install_dsh(tag: &str) -> anyhow::Result<DshRuntime> {
    let root = runtime_root();
    std::fs::create_dir_all(&root)?;
    let node = ensure_node(&root)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let staging = root.join(format!(".staging-{stamp}"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    eprintln!("[runtime] installing @deepseek-ai/dsh@{tag} into {}", staging.display());
    let mut npm_cmd = if let Some(cli) = ensure_npm(&root) {
        let mut command = tokio::process::Command::new(&node);
        command.arg(cli);
        command
    } else {
        tokio::process::Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
    };
    let output = npm_cmd
        .arg("install")
        .arg("--prefix")
        .arg(&staging)
        .arg("--no-audit")
        .arg("--no-fund")
        .arg(format!("@deepseek-ai/dsh@{tag}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run npm (is Node.js installed?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(anyhow!(
            "npm install failed: {}",
            stderr.chars().rev().take(1200).collect::<String>().chars().rev().collect::<String>()
        ));
    }

    let bin = staging.join("node_modules/@deepseek-ai/dsh/lib/bin.js");
    let version = version_from_bin(&bin).ok_or_else(|| anyhow!("installed package has no version"))?;

    // Smoke-test the profile we actually drive. If upstream removes or breaks
    // the SDK entry point, keep the previous runtime instead of switching.
    let smoke = tokio::process::Command::new(&node)
        .arg(&bin)
        .arg("--profile")
        .arg("sdk")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run the installed dsh SDK profile")?;
    if !smoke.status.success() {
        let stderr = String::from_utf8_lossy(&smoke.stderr);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(anyhow!(
            "dsh SDK profile smoke test failed: {}",
            stderr.chars().take(800).collect::<String>()
        ));
    }

    let final_dir = root.join(&version);
    if final_dir.exists() {
        // Never delete a runtime that might be in use by the running host.
        // Same version already installed: keep it and just select it.
        let _ = std::fs::remove_dir_all(&staging);
        let tmp = root.join(".current.tmp");
        std::fs::write(&tmp, &version)?;
        std::fs::rename(&tmp, root.join("current"))?;
        write_metadata(&root, &version)?;
        let bin = final_dir.join("node_modules/@deepseek-ai/dsh/lib/bin.js");
        return Ok(DshRuntime { version, dir: final_dir, node, bin });
    }
    std::fs::rename(&staging, &final_dir)?;
    let current = root.join("current");
    let tmp = root.join(".current.tmp");
    std::fs::write(&tmp, &version)?;
    std::fs::rename(&tmp, &current)?;
    write_metadata(&root, &version)?;
    eprintln!("[runtime] installed dsh {version} at {}", final_dir.display());
    let bin = final_dir.join("node_modules/@deepseek-ai/dsh/lib/bin.js");
    Ok(DshRuntime {
        version,
        dir: final_dir,
        node,
        bin,
    })
}
