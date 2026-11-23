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

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::{Host, Url};

/// 发布所在的仓库。
const REPO: &str = "xiaolikt/omd";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 取「最新 release」的 API 端点。`OMD_UPDATE_API` 可以指向本地假响应
/// （离线测试 / 企业镜像），默认走 github.com。
fn api_endpoint() -> String {
    std::env::var("OMD_UPDATE_API")
        .unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"))
}

fn loopback_http(url: &Url) -> bool {
    url.scheme() == "http"
        && match url.host() {
            Some(Host::Domain("localhost")) => true,
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        }
}

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("omd/{}", current_version()))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let from = &attempt.previous()[0];
            let to = attempt.url();
            if attempt.previous().len() >= 10 {
                attempt.error("too many update redirects")
            } else if to.scheme() == "https"
                || (loopback_http(from) && loopback_http(to) && from.origin() == to.origin())
            {
                attempt.follow()
            } else {
                attempt.error("insecure update redirect")
            }
        }))
        .build()?)
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    version: Version,
    pub notes: String,
    pub asset: Option<Asset>,
}

#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub size: Option<u64>,
    /// 发布页给的 sha256（GitHub release asset 的 `digest` 字段）。
    digest: Option<[u8; 32]>,
    /// 允许安装：仅 HTTPS 且带 sha256 的包；其余只能下载。
    pub installable: bool,
    url: Url,
}

/// `sha256:<64 hex>` → 32 字节；格式不对就当没有。
fn parse_sha256(value: &str) -> Option<[u8; 32]> {
    let hex = value.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

impl Release {
    pub fn is_newer(&self) -> bool {
        self.version
            .cmp_precedence(&Version::parse(current_version()).expect("Cargo package version"))
            .is_gt()
    }
}

fn parse_release(value: &Value, endpoint: &Url) -> Result<Release> {
    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let version = Version::parse(tag.strip_prefix(['v', 'V']).unwrap_or(&tag))
        .context("release tag is not a semantic version")?;
    let notes = value
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let expected = if cfg!(all(windows, target_arch = "x86_64")) {
        Some("omd-windows.zip".to_owned())
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(format!("omd-desktop_{version}_amd64.deb"))
    } else {
        None
    };
    let asset = expected.and_then(|name| {
        let entry = value
            .get("assets")?
            .as_array()?
            .iter()
            .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name.as_str()))?;
        let url = Url::parse(entry.get("browser_download_url")?.as_str()?).ok()?;
        if url.scheme() != "https"
            && !(loopback_http(endpoint)
                && loopback_http(&url)
                && endpoint.origin() == url.origin())
        {
            return None;
        }
        let size = entry.get("size").and_then(Value::as_u64).filter(|&n| n > 0);
        let digest = entry
            .get("digest")
            .and_then(Value::as_str)
            .and_then(parse_sha256);
        let https = endpoint.scheme() == "https" && url.scheme() == "https";
        Some(Asset {
            name,
            size,
            digest,
            installable: https && digest.is_some(),
            url,
        })
    });
    Ok(Release {
        version,
        tag,
        notes,
        asset,
    })
}

/// 拉最新 release。404 = 还没有发布过任何 release（提示语比错误更适合）。
pub async fn fetch_latest(client: &reqwest::Client) -> Result<Release> {
    let endpoint = Url::parse(&api_endpoint()).context("invalid update API URL")?;
    if endpoint.scheme() != "https" && !loopback_http(&endpoint) {
        bail!("the update API must use HTTPS or loopback HTTP");
    }
    let response = client
        .get(endpoint.clone())
        .timeout(Duration::from_secs(30))
        .header("accept", "application/vnd.github+json")
        .send()
        .await?;
    let status = response.status();
    if status.as_u16() == 404 {
        bail!("no releases published yet");
    }
    if !status.is_success() {
        bail!("update API returned HTTP {status}");
    }
    let text = response.text().await?;
    let value = serde_json::from_str(&text).context("update API sent unexpected JSON")?;
    parse_release(&value, &endpoint)
}

/// 下载目录：`~/.cache/omd/updates`。
pub fn download_dir() -> PathBuf {
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|base| base.is_absolute())
        .or_else(|| {
            std::env::home_dir()
                .filter(|home| home.is_absolute())
                .map(|home| {
                    if cfg!(windows) {
                        home.join("AppData/Local")
                    } else {
                        home.join(".cache")
                    }
                })
        })
    {
        base.join("omd").join("updates")
    } else {
        std::env::temp_dir()
    }
}

/// 流式下载，`on_progress(已下载, 总大小)` 每块都会调（总大小可能为 0）。
pub async fn apply(
    client: &reqwest::Client,
    asset: Asset,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<InstallOutcome> {
    use std::io::Write;

    let mut response = client.get(asset.url.clone()).send().await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("download failed: HTTP {}", response.status());
    }
    let total = asset.size.or(response.content_length());
    let dir = download_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let stage = tempfile::Builder::new().prefix("omd-").tempdir_in(dir)?;
    let dest = stage.path().join(&asset.name);
    let mut file = std::fs::File::create_new(&dest)?;
    let mut hasher = Sha256::new();
    let mut got: u64 = 0;
    while let Some(chunk) = response.chunk().await.context("download interrupted")? {
        got += chunk.len() as u64;
        if total.is_some_and(|total| got > total) {
            bail!("download exceeds the expected size");
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
        on_progress(got, total.unwrap_or(0));
    }
    if got == 0 || total.is_some_and(|total| got != total) {
        bail!("download did not match the expected size");
    }
    let actual: [u8; 32] = hasher.finalize().into();
    verify_digest(actual, asset.digest, asset.installable)?;
    file.sync_all()?;
    drop(file);
    tokio::task::spawn_blocking(move || {
        // Keep loopback HTTP fixtures and Windows ZIPs download-only.
        let outcome = if asset.installable && cfg!(target_os = "linux") {
            install(&dest)
        } else {
            Ok(InstallOutcome::Downloaded(dest.clone()))
        };
        if !matches!(outcome, Ok(InstallOutcome::Installed(_))) {
            let _ = stage.keep();
        }
        outcome
    })
    .await?
}

/// 只有摘要对上（或本来就只能手动下载）才允许继续。
fn verify_digest(actual: [u8; 32], expected: Option<[u8; 32]>, installable: bool) -> Result<()> {
    match expected {
        Some(expected) if actual != expected => bail!("download failed the sha256 check"),
        Some(_) => Ok(()),
        None if installable => bail!("release asset has no sha256 digest"),
        None => Ok(()),
    }
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
        .status()
        .with_context(|| format!("cannot run {cmd:?}"))
}

/// 用户态安装：把 deb 里的 payload 覆盖到 `opt_dir`。
///
#[cfg(target_os = "linux")]
fn install_into(deb: &Path, opt_dir: &Path) -> Result<InstallOutcome> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    use std::os::unix::fs::PermissionsExt;

    let parent = opt_dir.parent().context("missing installation parent")?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(".omd-desktop.lock"))?;
    lock.try_lock()
        .context("another user installation is in progress")?;
    let stage = tempfile::Builder::new()
        .prefix(".omd-update-")
        .tempdir_in(parent)?;
    let status = run(std::process::Command::new("dpkg-deb")
        .arg("-x")
        .arg(deb)
        .arg(stage.path()))?;
    if !status.success() {
        bail!("extracting the update failed ({status}); the installed app was not changed");
    }
    let payload = stage.path().join("usr/lib/omd-desktop");
    let omd = payload
        .join("omd")
        .symlink_metadata()
        .context("package has no OMD binary")?;
    if !omd.is_file() || omd.permissions().mode() & 0o111 == 0 {
        bail!("package has no executable OMD binary");
    }
    // Both paths are on the install filesystem. A failed exchange leaves the old tree in place.
    renameat_with(CWD, &payload, CWD, opt_dir, RenameFlags::EXCHANGE)
        .context("switching installations; the installed app was not changed")?;
    Ok(InstallOutcome::Installed(format!(
        "installed into {} — restart OMD to use it",
        opt_dir.display()
    )))
}

/// 根据安装位置决定怎么装。
fn install(deb: &Path) -> Result<InstallOutcome> {
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = exe.canonicalize().unwrap_or(exe);
    let exe_dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();

    // 1) 用户态安装（install-user.sh / 自更新装出来的）
    #[cfg(target_os = "linux")]
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
            return match run(&mut cmd)?.code() {
                Some(0) => Ok(InstallOutcome::Installed(
                    "system package updated — restart OMD to use it".to_string(),
                )),
                Some(126) => Ok(InstallOutcome::Downloaded(deb.to_path_buf())),
                code => bail!(
                    "system installer failed ({code:?}); package: {}",
                    deb.display()
                ),
            };
        }
        // 没有 pkexec：把包交给软件中心
        if which("xdg-open").is_some() {
            let status = run(std::process::Command::new("xdg-open").arg(deb))?;
            if !status.success() {
                bail!(
                    "software installer failed ({status}); package: {}",
                    deb.display()
                );
            }
            return Ok(InstallOutcome::Delegated(format!(
                "opened {} in the software installer",
                deb.display()
            )));
        }
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
        let endpoint = Url::parse("https://example.test").unwrap();
        let running = Version::parse(current_version()).unwrap();
        let next = format!("v{}.{}.{}", running.major + 1, 0, 0);
        assert!(parse_release(&json!({"tag_name": next}), &endpoint)
            .unwrap()
            .is_newer());
        assert!(
            !parse_release(&json!({"tag_name": format!("v{running}")}), &endpoint)
                .unwrap()
                .is_newer()
        );
        assert!(parse_release(&json!({"tag_name": "v1.bad.0"}), &endpoint).is_err());
        let rc = Version::parse("1.0.0-rc.1").unwrap();
        assert!(parse_release(&json!({"tag_name": "v1.0.0"}), &endpoint)
            .unwrap()
            .version
            .cmp_precedence(&rc)
            .is_gt());
    }

    #[test]
    fn parses_release_and_picks_platform_asset() {
        let value = json!({
            "tag_name": "v0.2.0",
            "body": "- empty spaces\n- updater\n",
            "assets": [
                { "name": "omd-desktop-debug_0.2.0_amd64.deb",
                  "browser_download_url": "https://example.test/debug",
                  "size": 999000000u64 },
                { "name": "omd-desktop_0.2.0_amd64.deb",
                  "browser_download_url": "https://example.test/omd-desktop_0.2.0_amd64.deb",
                  "size": 155000000u64,
                  "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
                { "name": "omd-windows.zip",
                  "browser_download_url": "https://example.test/omd-windows.zip",
                  "size": 204000000u64,
                  "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" }
            ]
        });
        let release =
            parse_release(&value, &Url::parse("https://example.test").unwrap()).expect("release");
        assert_eq!(release.version.to_string(), "0.2.0");
        assert_eq!(release.tag, "v0.2.0");
        assert!(release.notes.contains("updater"));
        if cfg!(all(windows, target_arch = "x86_64")) {
            let asset = release.asset.unwrap();
            assert_eq!(asset.name, "omd-windows.zip");
            assert!(asset.installable);
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            let asset = release.asset.unwrap();
            assert_eq!(asset.name, "omd-desktop_0.2.0_amd64.deb");
            assert_eq!(asset.size, Some(155_000_000));
            assert!(asset.installable);
        } else {
            assert!(release.asset.is_none());
        }
    }

    #[test]
    fn parses_sha256_digest() {
        let hex = "ab".repeat(32);
        assert_eq!(parse_sha256(&format!("sha256:{hex}")).unwrap(), [0xab; 32]);
        assert!(parse_sha256(&format!("sha256:{}", "zz".repeat(32))).is_none());
        assert!(parse_sha256(&format!("sha512:{hex}")).is_none());
        assert!(parse_sha256("sha256:abc").is_none());
    }

    #[test]
    fn digest_mismatch_or_missing_blocks_install() {
        let actual = [1u8; 32];
        assert!(verify_digest(actual, Some([1u8; 32]), true).is_ok());
        assert!(verify_digest(actual, Some([2u8; 32]), true).is_err());
        assert!(verify_digest(actual, None, true).is_err());
        // 本来就只能手动下载的包（loopback / Windows）不要求摘要。
        assert!(verify_digest(actual, None, false).is_ok());
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn asset_without_digest_is_download_only() {
        let release = parse_release(
            &json!({
                "tag_name": "v9.0.0",
                "assets": [{
                    "name": "omd-desktop_9.0.0_amd64.deb",
                    "browser_download_url": "https://example.test/package",
                }],
            }),
            &Url::parse("https://example.test").unwrap(),
        )
        .unwrap();
        assert!(!release.asset.unwrap().installable);
    }

    #[test]
    fn release_without_asset_still_parses() {
        let value = json!({ "tag_name": "v9.9.9", "assets": [] });
        let release =
            parse_release(&value, &Url::parse("https://example.test").unwrap()).expect("release");
        assert!(release.asset.is_none());
        assert_eq!(release.version.to_string(), "9.9.9");
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn loopback_metadata_cannot_enable_installation() {
        let endpoint = Url::parse("http://127.0.0.1:8080/latest").unwrap();
        let release = |url: &str| {
            parse_release(
                &json!({
                    "tag_name": "v9.0.0",
                    "assets": [{
                        "name": "omd-desktop_9.0.0_amd64.deb",
                        "browser_download_url": url,
                    }],
                }),
                &endpoint,
            )
            .unwrap()
        };
        assert!(
            !release("http://127.0.0.1:8080/package")
                .asset
                .unwrap()
                .installable
        );
        assert!(
            !release("https://example.test/package")
                .asset
                .unwrap()
                .installable
        );
        assert!(release("http://127.0.0.1:8081/package").asset.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn user_update_keeps_old_payload_until_valid_replacement() {
        use std::{fs, os::unix::fs::PermissionsExt, process::Command};

        if Command::new("dpkg-deb").arg("--version").output().is_err() {
            eprintln!("skipping Debian install test: dpkg-deb unavailable");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let old = work.path().join("omd-desktop");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("omd"), "old binary").unwrap();
        let source = work.path().join("package");
        let payload = source.join("usr/lib/omd-desktop");
        fs::create_dir_all(&payload).unwrap();
        fs::create_dir(source.join("DEBIAN")).unwrap();
        fs::write(source.join("DEBIAN/control"),
            "Package: omd-desktop\nVersion: 1.0.0\nArchitecture: all\nMaintainer: Test <test@example.test>\nDescription: updater fixture\n").unwrap();
        fs::write(payload.join("omd"), "new binary").unwrap();
        let deb = work.path().join("update.deb");
        let pack = || {
            assert!(Command::new("dpkg-deb")
                .arg("--build")
                .arg(&source)
                .arg(&deb)
                .output()
                .unwrap()
                .status
                .success());
        };
        pack();
        assert!(install_into(&deb, &old).is_err());
        assert_eq!(fs::read_to_string(old.join("omd")).unwrap(), "old binary");

        fs::set_permissions(payload.join("omd"), fs::Permissions::from_mode(0o755)).unwrap();
        pack();
        let lock = fs::OpenOptions::new()
            .write(true)
            .open(work.path().join(".omd-desktop.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(install_into(&deb, &old).is_err());
        assert_eq!(fs::read_to_string(old.join("omd")).unwrap(), "old binary");
        drop(lock);

        assert!(matches!(
            install_into(&deb, &old).unwrap(),
            InstallOutcome::Installed(_)
        ));
        assert_eq!(fs::read_to_string(old.join("omd")).unwrap(), "new binary");
    }
}
