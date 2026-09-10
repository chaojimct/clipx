//! FindX 检索服务客户端：JSON 行协议 + `pinyin: true`（与 findx2-ipc 同一条协议）。
//!
//! 传输层按平台：Windows 命名管道；macOS/Linux Unix domain socket
//! （路径规则与 findx2-ipc 一致：mac `~/Library/Application Support/FindX/{name}.sock`、
//! linux `$XDG_RUNTIME_DIR/{name}.sock`，env/设置里的绝对路径原样用）。
//! Everything WM_COPYDATA 没有拼音，搜不到「马春天」这类中文名，故 FindX 优先。

use std::io::{BufRead, BufReader, Read, Write};
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
    names.into_iter().map(endpoint_for).collect()
}

/// 候选名 → 端点路径。Windows：命名管道；Unix：绝对路径原样，纯名字走 sock 规则。
#[cfg(windows)]
fn endpoint_for(n: String) -> String {
    if n.starts_with(r"\\") {
        n
    } else {
        format!(r"\\.\pipe\{n}")
    }
}

#[cfg(unix)]
fn endpoint_for(n: String) -> String {
    if n.contains('/') {
        n
    } else {
        unix_socket_path(&n)
    }
}

/// 与 findx2-ipc 的 `unix_socket_path` 同规则的 socket 路径。
#[cfg(unix)]
fn unix_socket_path(name: &str) -> String {
    let trimmed = name.trim();
    let file = if trimmed.is_empty() {
        "findx2.sock".to_string()
    } else if trimmed.ends_with(".sock") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.sock")
    };
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let dir = std::path::PathBuf::from(home).join("Library/Application Support/FindX");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(file).to_string_lossy().to_string()
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            if !runtime.is_empty() {
                return std::path::PathBuf::from(runtime)
                    .join(file)
                    .to_string_lossy()
                    .to_string();
            }
        }
        std::env::temp_dir().join(file).to_string_lossy().to_string()
    }
}

/// findx GUI 设置里的管道名（findx2-gui-settings.json 的 pipeName）。
fn pipe_name_from_settings() -> Option<String> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(std::path::PathBuf::from)?;
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join("Library/Application Support"))?;
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    let p = base.join("tools.findx.gui").join("findx2-gui-settings.json");
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
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("sc.exe")
            .args(["start", "FindX2Search"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // 服务未注册进 PATH 时静默失败；GUI 侧自会管理服务生命周期。
        let _ = std::process::Command::new("findx2-service")
            .arg("-startup")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    std::thread::sleep(Duration::from_millis(800));
}

/// 启动时预热：端点已通则只缓存，否则再拉 FindX 服务。不要放进按键查询路径。
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

/// 打开一个可读写端点。Windows：命名管道句柄；Unix：连接 UDS。
#[cfg(windows)]
fn open_stream(path: &str) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(true).open(path)
}

#[cfg(unix)]
fn open_stream(path: &str) -> std::io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(path)
}

/// 单端点搜索：JSON 行请求 → 单行 JSON 响应（协议与 findx2-ipc 一致）。
fn search_one_stream<S: Read + Write>(
    mut f: S,
    query: &str,
    limit: u32,
) -> Result<QueryResults, QueryError> {
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

fn search_one_pipe(path: &str, query: &str, limit: u32) -> Result<QueryResults, QueryError> {
    let f = open_stream(path).map_err(|_| QueryError::NotRunning)?;
    search_one_stream(f, query, limit)
}

/// 连接 FindX 服务端点并搜索（启用拼音）。端点不存在 → `NotRunning`。
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

    #[cfg(unix)]
    #[test]
    fn search_stream_roundtrip_over_unix_socket() {
        // 本地 UDS 服务端回放固定 JSON 行，验证客户端收发与解析（协议回环）。
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("clipx-fp-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("t.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind");
        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut line = String::new();
                let _ = std::io::BufReader::new(&mut conn).read_line(&mut line);
                let reply = serde_json::json!({
                    "type": "search_result",
                    "total": 1,
                    "hits": [{"name": "a.txt", "path": "/tmp/a.txt", "is_directory": false}]
                })
                .to_string();
                let _ = conn.write_all(reply.as_bytes());
                let _ = conn.write_all(b"\n");
            }
        });
        let f = std::os::unix::net::UnixStream::connect(&sock).expect("connect");
        let r = search_one_stream(f, "a", 10).expect("search");
        server.join().ok();
        let _ = std::fs::remove_file(&sock);
        assert_eq!(r.items[0].file_name, "a.txt");
    }

    #[cfg(unix)]
    #[test]
    fn socket_path_rules_match_findx2_ipc() {
        assert_eq!(unix_socket_path("/tmp/x.sock"), "/tmp/x.sock");
        assert!(unix_socket_path("myname").ends_with("myname.sock"));
    }
}
