//! 从 GitHub Releases 自更新。
//!
//! 版本号来自 `Cargo.toml`（`env!("CARGO_PKG_VERSION")`），发布产物约定：
//! `omd-desktop_<version>_amd64.deb`（Linux）/ `omd-windows.zip`（Windows）。
//!
//! 安装方式按当前可执行文件的位置分派：
//! - `~/.local/opt/omd-desktop/`（`packaging/install-user.sh` 装的）→ 无 root
//!   解包覆盖自己，重启即生效；
//! - `/usr/lib/omd-desktop/`（deb 装的）→ `pkexec dpkg -i`，走系统授权；
//! - 其它（`cargo run`、`target/release`）→ 只下载，提示手动安装。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

/// 发布所在的仓库。
pub const REPO: &str = "xiaolikt/omd";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 取「最新 release」的 API 端点。`OMD_UPDATE_API` 可以指向本地假响应
/// （离线测试 / 企业镜像），默认走 github.com。
fn api_endpoint() -> String {
    std::env::var("OMD_UPDATE_API")
        .unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"))
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub notes: String,
    pub html_url: String,
    /// 当前平台的安装包；没有对应产物时为空
    pub asset_name: String,
    pub asset_url: String,
    pub asset_size: u64,
}

impl Release {
    pub fn has_asset(&self) -> bool {
        !self.asset_url.is_empty()
    }
}

/// `v0.1.2` / `0.1.2` → `0.1.2`。
fn strip_tag(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// 只比较 `major.minor.patch`（忽略 `-rc.1` 这种后缀），数值逐段比。
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parts(latest), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// 当前平台该拿哪个产物。
fn asset_matches(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if cfg!(windows) {
        name.ends_with("omd-windows.zip") || name.ends_with("omd.exe")
    } else {
        name.ends_with("_amd64.deb") || name.ends_with("_x86_64.deb")
    }
}

/// GitHub release JSON → `Release`。抽成纯函数方便用固定 JSON 测。
pub fn parse_release(value: &Value) -> Result<Release> {
    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if tag.is_empty() {
        bail!("release has no tag_name");
    }
    let notes = value
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let html_url = value
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let assets = value
        .get("assets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut picked: Option<(String, String, u64)> = None;
    for asset in assets {
        let name = asset
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !asset_matches(&name) {
            continue;
        }
        // 同一平台有多种产物时取最大的（通常就是完整包）
        let size = asset.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let url = asset
            .get("browser_download_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if url.is_empty() {
            continue;
        }
        if picked.as_ref().map_or(true, |(_, _, s)| size > *s) {
            picked = Some((name, url, size));
        }
    }
    let (asset_name, asset_url, asset_size) = picked.unwrap_or_default();
    Ok(Release {
        version: strip_tag(&tag),
        tag,
        notes,
        html_url,
        asset_name,
        asset_url,
        asset_size,
    })
}

/// 拉最新 release。404 = 还没有发布过任何 release（提示语比错误更适合）。
pub async fn fetch_latest(client: &reqwest::Client) -> Result<Release> {
    let response = client
        .get(api_endpoint())
        .header("accept", "application/vnd.github+json")
        .header("user-agent", format!("omd/{}", current_version()))
        .send()
        .await
        .map_err(|err| anyhow!("cannot reach github: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| anyhow!("reading the release failed: {err}"))?;
    if status.as_u16() == 404 {
        bail!("no releases published yet");
    }
    if !status.is_success() {
        bail!("github returned HTTP {status}: {}", text.chars().take(160).collect::<String>());
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|err| anyhow!("github sent unexpected json: {err}"))?;
    parse_release(&value)
}

/// 下载目录：`~/.cache/omd/updates`。
pub fn download_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join("omd").join("updates");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 流式下载，`on_progress(已下载, 总大小)` 每块都会调（总大小可能为 0）。
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dir: &Path,
    file_name: &str,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    use std::io::Write;

    let response = client
        .get(url)
        .header("user-agent", format!("omd/{}", current_version()))
        .send()
        .await
        .map_err(|err| anyhow!("download failed: {err}"))?;
    if !response.status().is_success() {
        bail!("download failed: HTTP {}", response.status());
    }
    let total = response.content_length().unwrap_or(0);
    let dest = dir.join(file_name);
    let tmp = dest.with_extension("part");
    let mut file = std::fs::File::create(&tmp)
        .map_err(|err| anyhow!("cannot write {}: {err}", tmp.display()))?;
    let mut response = response;
    let mut got: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| anyhow!("download interrupted: {err}"))?
    {
        file.write_all(&chunk)
            .map_err(|err| anyhow!("cannot write {}: {err}", tmp.display()))?;
        got += chunk.len() as u64;
        on_progress(got, total);
    }
    file.flush().ok();
    drop(file);
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// 更新怎么落地。
#[derive(Clone, Debug)]
pub enum InstallOutcome {
    /// 装完了，重启 app 生效
    Installed(String),
    /// 已交给系统授权 / 软件中心，用户确认后生效
    Delegated(String),
    /// 只下载了，需要用户手动处理
    Downloaded(PathBuf),
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

fn run(cmd: &mut std::process::Command) -> Result<std::process::ExitStatus> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|err| anyhow!("cannot run {:?}: {err}", cmd))
}

/// 用户态安装：把 deb 里的 payload 覆盖到 `opt_dir`。
///
/// 运行中的进程映射的是旧 inode，`rm -rf` + 重新落盘不会影响它；
/// 下一次启动就是新版本。
fn install_into(deb: &Path, opt_dir: &Path) -> Result<InstallOutcome> {
    let stage = std::env::temp_dir().join(format!("omd-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    let status = run(
        std::process::Command::new("dpkg-deb")
            .arg("-x")
            .arg(deb)
            .arg(&stage),
    );
    let status = match status {
        Ok(status) => status,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(err);
        }
    };
    let payload = stage.join("usr/lib/omd-desktop");
    if !status.success() || !payload.join("omd").exists() {
        let _ = std::fs::remove_dir_all(&stage);
        bail!("unexpected package layout (no usr/lib/omd-desktop/omd)");
    }
    let _ = std::fs::remove_dir_all(opt_dir);
    std::fs::create_dir_all(opt_dir)?;
    let copied = run(
        std::process::Command::new("cp")
            .arg("-a")
            .arg(format!("{}/.", payload.display()))
            .arg(opt_dir),
    );
    let _ = std::fs::remove_dir_all(&stage);
    copied?;
    Ok(InstallOutcome::Installed(format!(
        "installed into {} — restart OMD to use it",
        opt_dir.display()
    )))
}

/// 根据安装位置决定怎么装。
pub fn install(deb: &Path) -> Result<InstallOutcome> {
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = exe.canonicalize().unwrap_or(exe);
    let exe_dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();

    // 1) 用户态安装（install-user.sh / 自更新装出来的）
    if exe_dir
        .file_name()
        .is_some_and(|name| name == "omd-desktop")
        && exe_dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "opt")
    {
        return install_into(deb, &exe_dir);
    }

    // 2) 系统 deb 安装
    if exe.starts_with("/usr/lib/omd-desktop") || exe.starts_with("/usr/lib/omd") {
        if which("pkexec").is_some() {
            let mut cmd = std::process::Command::new("pkexec");
            cmd.arg("dpkg").arg("-i").arg(deb);
            return match run(&mut cmd) {
                Ok(status) if status.success() => Ok(InstallOutcome::Installed(
                    "system package updated — restart OMD to use it".to_string(),
                )),
                _ => Ok(InstallOutcome::Delegated(format!(
                    "authorization cancelled; install it manually: sudo dpkg -i {}",
                    deb.display()
                ))),
            };
        }
        // 没有 pkexec：把包交给软件中心
        if which("xdg-open").is_some() {
            let _ = run(std::process::Command::new("xdg-open").arg(deb));
            return Ok(InstallOutcome::Delegated(format!(
                "opened {} in the software installer",
                deb.display()
            )));
        }
        return Ok(InstallOutcome::Downloaded(deb.to_path_buf()));
    }

    // 3) 开发构建：只下载
    Ok(InstallOutcome::Downloaded(deb.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        // tag 前缀与预发布后缀
        assert!(is_newer("0.1.2-rc.1", "0.1.1"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
    }

    #[test]
    fn parses_release_and_picks_platform_asset() {
        let value = json!({
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/xiaolikt/omd/releases/tag/v0.2.0",
            "body": "- empty spaces\n- updater\n",
            "assets": [
                { "name": "omd-desktop_0.2.0_amd64.deb",
                  "browser_download_url": "https://example.test/omd-desktop_0.2.0_amd64.deb",
                  "size": 155000000u64 },
                { "name": "omd-windows.zip",
                  "browser_download_url": "https://example.test/omd-windows.zip",
                  "size": 204000000u64 }
            ]
        });
        let release = parse_release(&value).expect("release");
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.tag, "v0.2.0");
        assert!(release.notes.contains("updater"));
        assert!(release.has_asset());
        if cfg!(windows) {
            assert_eq!(release.asset_name, "omd-windows.zip");
        } else {
            assert_eq!(release.asset_name, "omd-desktop_0.2.0_amd64.deb");
            assert_eq!(release.asset_size, 155_000_000);
        }
    }

    #[test]
    fn release_without_asset_still_parses() {
        let value = json!({ "tag_name": "v9.9.9", "assets": [] });
        let release = parse_release(&value).expect("release");
        assert!(!release.has_asset());
        assert_eq!(release.version, "9.9.9");
    }
}
