//! FindX 命名管道（与 GUI 同一条协议）：JSON 行 + `pinyin: true`。
//! Everything WM_COPYDATA 没有拼音，搜不到「马春天」这类中文名。

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::{QueryError, QueryResults, ResultItem};

const DEFAULT_PIPES: &[&str] = &["findx2", "FindX", "findx"];
static LAST_PIPE: Mutex<Option<String>> = Mutex::new(None);

fn pipe_candidates() -> Vec<String> {
    let mut names = Vec::new();
    for key in ["FINDX2_PIPE", "FINDX_PIPE"] {
        if let Ok(env) = std::env::var(key) {
            let t = env.trim();
            if !t.is_empty() {
                names.push(t.to_string());
            }
        }
    }
    if let Some(n) = pipe_name_from_settings() {
        names.push(n);
    }
    for n in DEFAULT_PIPES {
        if !names.iter().any(|x| x.eq_ignore_ascii_case(n)) {
            names.push((*n).to_string());
        }
    }
    names
        .into_iter()
        .map(|n| {
            if n.starts_with(r"\\") {
                n
            } else {
                format!(r"\\.\pipe\{n}")
            }
        })
        .collect()
}

fn pipe_name_from_settings() -> Option<String> {
    let appdata = std::env::var_os("APPDATA")?;
    let p = std::path::PathBuf::from(appdata)
        .join("tools.findx.gui")
        .join("findx2-gui-settings.json");
    let text = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let n = v.get("pipeName").and_then(|x| x.as_str())?.trim();
    if n.is_empty() {
        None
    } else {
        Some(n.to_string())
    }
}

fn ordered_pipes() -> Vec<String> {
    let mut names = pipe_candidates();
    if let Ok(g) = LAST_PIPE.lock() {
        if let Some(p) = g.as_ref() {
            names.retain(|x| x != p);
            names.insert(0, p.clone());
        }
    }
    names
}

fn remember_pipe(path: &str) {
    if let Ok(mut g) = LAST_PIPE.lock() {
        *g = Some(path.to_string());
    }
}

pub(crate) fn has_cached_pipe() -> bool {
    LAST_PIPE.lock().ok().and_then(|g| g.clone()).is_some()
}

fn try_start_findx_service() {
    static ONCE: AtomicBool = AtomicBool::new(false);
    if ONCE.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::process::Command::new("sc.exe")
        .args(["start", "FindX2Search"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    std::thread::sleep(Duration::from_millis(800));
}

/// 启动时预热：管道已通则只缓存，否则再拉 FindX 服务。不要放进按键查询路径。
pub fn warmup() {
    if query("windows", 8, Duration::from_millis(200)).is_ok() {
        return;
    }
    try_start_findx_service();
    let _ = query("windows", 8, Duration::from_millis(400));
}

fn parse_name_hl(v: Option<&serde_json::Value>) -> Vec<(u32, u32)> {
    let Some(arr) = v.and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for span in arr {
        let Some(pair) = span.as_array() else {
            continue;
        };
        if pair.len() < 2 {
            continue;
        }
        let Some(s) = pair[0].as_u64() else { continue };
        let Some(e) = pair[1].as_u64() else { continue };
        if e > s {
            out.push((s as u32, e as u32));
        }
    }
    out
}

fn hits_from_json(v: &serde_json::Value) -> Option<QueryResults> {
    if v.get("type").and_then(|t| t.as_str()) != Some("search_result") {
        return None;
    }
    let arr = v.get("hits")?.as_array()?;
    let mut items = Vec::with_capacity(arr.len());
    let mut folders = 0u32;
    let mut files = 0u32;
    for h in arr {
        let name = h
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let path = h
            .get("path")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if path.is_empty() && name.is_empty() {
            continue;
        }
        let is_folder = h
            .get("is_directory")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        if is_folder {
            folders += 1;
        } else {
            files += 1;
        }
        let file_name = if name.is_empty() {
            path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
        } else {
            name
        };
        let full_path = if path.is_empty() {
            file_name.clone()
        } else {
            path
        };
        items.push(ResultItem {
            full_path,
            file_name,
            is_folder,
            is_drive: false,
            name_hl: parse_name_hl(h.get("name_highlight")),
        });
    }
    let n = items.len() as u32;
    Some(QueryResults {
        total_items: v
            .get("total")
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
            .unwrap_or(n),
        total_folders: folders,
        total_files: files,
        items,
    })
}

fn search_one_pipe(path: &str, query: &str, limit: u32) -> Result<QueryResults, QueryError> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| QueryError::NotRunning)?;
    let body = serde_json::json!({
        "type": "search",
        "query": query,
        "pinyin": true,
        "limit": limit.max(1),
        "offset": 0,
    })
    .to_string();
    f.write_all(body.as_bytes())
        .and_then(|_| f.write_all(b"\n"))
        .and_then(|_| f.flush())
        .map_err(|_| QueryError::SendFailed)?;
    let mut line = String::new();
    BufReader::new(f)
        .read_line(&mut line)
        .map_err(|_| QueryError::InvalidReply)?;
    let v: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|_| QueryError::InvalidReply)?;
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        return Err(QueryError::InvalidReply);
    }
    hits_from_json(&v).ok_or(QueryError::InvalidReply)
}

/// 连接 FindX 服务管道并搜索（启用拼音）。管道不存在 → `NotRunning`。
/// 不在查询路径上拉服务（那会卡 1s+）；预热在 [`warmup`] 完成。
pub fn query(search: &str, max_results: u32, timeout: Duration) -> Result<QueryResults, QueryError> {
    let q = search.to_string();
    let max = max_results.clamp(1, 10_000);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("clipx-findx-pipe".into())
        .spawn(move || {
            let mut last = QueryError::NotRunning;
            for path in ordered_pipes() {
                match search_one_pipe(&path, &q, max) {
                    Ok(r) => {
                        remember_pipe(&path);
                        let _ = tx.send(Ok(r));
                        return;
                    }
                    Err(e) => last = e,
                }
            }
            let _ = tx.send(Err(last));
        })
        .map_err(|_| QueryError::ReplyWindowFailed)?;
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => Err(QueryError::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_findx_search_result() {
        let v: serde_json::Value = serde_json::json!({
            "type": "search_result",
            "total": 1,
            "hits": [{
                "name": "马春天.pdf",
                "path": r"D:\docs\马春天.pdf",
                "is_directory": false,
                "name_highlight": [[0, 3]]
            }]
        });
        let r = hits_from_json(&v).expect("parse");
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].file_name, "马春天.pdf");
        assert_eq!(r.items[0].name_hl, vec![(0, 3)]);
        assert!(!r.items[0].is_folder);
    }
}
