//! Local session index + history replay from dsh's persisted session logs.
//!
//! This replaces the web API's `session/list` and snapshot replay when the GUI
//! speaks the SDK protocol directly. The on-disk format is versioned (`v4`);
//! keep the parser tolerant so a newer format degrades to "skip" rather than
//! crashing the GUI.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub blank: bool,
    pub path: PathBuf,
}

pub fn dsh_home() -> PathBuf {
    if let Ok(home) = std::env::var("DSH_HOME") {
        return PathBuf::from(home);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".dsh")
}

fn open_log(path: &Path) -> Result<Box<dyn Read>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("open session log {}", path.display()))?;
    if path.extension().and_then(|ext| ext.to_str()) == Some("zstd") {
        Ok(Box::new(zstd::stream::read::Decoder::new(file)?))
    } else {
        Ok(Box::new(file))
    }
}

pub fn load_events(path: &Path) -> Result<Vec<Value>> {
    let reader = BufReader::new(open_log(path)?);
    let mut events = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        events.push(value);
    }
    Ok(events)
}

fn read_session(path: &Path) -> Option<SessionInfo> {
    let metadata = std::fs::metadata(path).ok()?;
    let updated_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let reader = BufReader::new(open_log(path).ok()?);
    let mut id = None;
    let mut cwd = None;
    let mut title = String::new();
    let mut created_at = 0;
    let mut blank = true;
    for line in reader.lines().take(600) {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(|v| v.as_str()) {
            Some("session") => {
                id = value.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                cwd = value.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string());
                created_at = value
                    .get("createdAt")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
            }
            Some("session/title") => {
                if let Some(text) = value
                    .pointer("/data/title")
                    .and_then(|v| v.as_str())
                    .filter(|text| !text.trim().is_empty())
                {
                    title = text.to_string();
                }
            }
            Some("user/message") => {
                if value
                    .pointer("/data/source/kind")
                    .and_then(|v| v.as_str())
                    == Some("user")
                {
                    blank = false;
                }
            }
            _ => {}
        }
    }
    let id = id.or_else(|| {
        path.parent()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .map(|s| s.to_string())
    })?;
    let cwd = cwd.unwrap_or_else(|| "/".to_string());
    Some(SessionInfo {
        id,
        cwd,
        title,
        created_at,
        updated_at,
        blank,
        path: path.to_path_buf(),
    })
}

pub fn scan_sessions() -> Result<Vec<SessionInfo>> {
    let root = dsh_home().join("sessions");
    let mut sessions = Vec::new();
    let Ok(projects) = std::fs::read_dir(&root) else {
        return Ok(sessions);
    };
    for project in projects.flatten() {
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let candidates = [
                dir.join("session.v4.jsonl.zstd"),
                dir.join("session.jsonl.zstd"),
                dir.join("session.v4.jsonl"),
                dir.join("session.jsonl"),
            ];
            let Some(path) = candidates.iter().find(|path| path.is_file()) else {
                continue;
            };
            if let Some(info) = read_session(path) {
                sessions.push(info);
            }
        }
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    Ok(sessions)
}
