//! `bw serve` 常驻进程：一次冷启动（~3.5s）换来之后全部毫秒级。
//!
//! 实测（本机 2026-11-04）：CLI 每次 spawn 冷启动 3.5s，一次登录要 4 次 spawn ≈
//! 15s；serve 启动后 status 46ms、unlock（含 PBKDF2）约 0.6s、sync 约 1s。
//!
//! **前提：serve 只在已登录（含锁定）态可用**——OSS 构建 2026.10.1 在未登录态
//! 启动即退（`You are not logged in.`，2026-11-20 实测）。故本模块对启动即退
//! 快速失败 + 记账（[`SPAWN_DEAD`]），登录/解锁成功后由调用方清账再试；
//! 未登录的探活/登录操作一律先走 CLI，登录成功后才拉 daemon。
//!
//! 高频操作（status/unlock/lock/sync/list/get）走 HTTP；serve 没暴露的操作
//!（login/config server 在此 bw 版本 404）继续走 CLI spawn。
//! 会话由 daemon 进程持有——HTTP 侧无状态本进程不再需要在线程间传递 BW_SESSION。
//!
//! daemon 生命周期：`ensure_client` 首次调用时起（后台线程一次）；
//! 退出时 `shutdown` 杀掉自己起的那个。端口 8087 起向下找空位锁文件记
//! `Data/bw_serve.json`（port+pid）认领疑似自己的孤儿。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::model::{parse_list, parse_status, VaultItem};

/// 找空端口区间（默认 8087 起最多试到 8099）。
const PORT_START: u16 = 8087;
const PORT_END: u16 = 8099;
/// daemon 冷启动等待上限（node 二进制一次 ~3.5s留足余量）。
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
/// HTTP 单次上限（localhost 正常是毫秒级；兜底防 daemon 僵死时无限卡）。
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// 逻辑线程上只允许速战速决（daemon 僵死时最多堵 2.5s 不冻整个应用）。
const HTTP_TIMEOUT_FAST: Duration = Duration::from_millis(2500);
/// 读出上限（list 大 vault 也到不了 1MB；防异常撑内存与 CLI 端 MAX_STDOUT 对齐）。
const BODY_LIMIT: u64 = 1024 * 1024;

struct ServeState {
    base: String,
    owned: Option<std::process::Child>,
}

fn slot() -> &'static Mutex<Option<ServeState>> {
    static SERVE: OnceLock<Mutex<Option<ServeState>>> = OnceLock::new();
    SERVE.get_or_init(|| Mutex::new(None))
}

fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        // serve 出错回 4xx + JSON envelope（{"success":false,"message":...}）
        // 必须自己读 body不能让 ureq 按错码直接抛（否则丢了原文）。
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build();
    ureq::Agent::new_with_config(config)
}

/// 只读已启动的 daemon（不开机不探活不阻塞；逻辑线程与后台线程都能调）。
/// daemon 没起返回 None调用方走 CLI 老路。
pub fn try_client() -> Option<ServeClient> {
    try_client_with(HTTP_TIMEOUT)
}

/// 逻辑线程专用：超时收紧到 2.5s（daemon 僵死也不冻 UI）。
pub fn try_client_fast() -> Option<ServeClient> {
    try_client_with(HTTP_TIMEOUT_FAST)
}

fn try_client_with(timeout: Duration) -> Option<ServeClient> {
    let guard = slot().lock().ok()?;
    let st = guard.as_ref()?;
    Some(ServeClient {
        base: st.base.clone(),
        agent: agent(timeout),
    })
}

/// 上次 `bw serve` 启动即退（如**未登录态**：OSS 构建 2026.10.1 实测 `You are
/// not logged in.` 即退）——置位后跳过重复尝试，直到登录/解锁成功调
/// [`mark_serve_ok`] 清除。防的是每次探活/操作都白付一次 node 冷启动
/// + 退出 + 30 秒等待的循环（首版正是这样把「登录」按钮拖没的）。
static SPAWN_DEAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 登录/解锁成功的调用方清「serve 不可用」记忆（状态变了，daemon 值得再试）。
pub fn mark_serve_ok() {
    SPAWN_DEAD.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// 确保 daemon 在（没起就起；调用方必须是后台线程首次约 4s）。
/// `data_dir` 放锁文件；`exe` 是 bw 二进制路径。
pub fn ensure_client(data_dir: &Path, exe: &Path) -> Result<ServeClient, String> {
    if let Some(c) = try_client() {
        return Ok(c);
    }
    if SPAWN_DEAD.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("bw serve 当前起不来（未登录态即退），登录/解锁成功后自动恢复".into());
    }
    let mut guard = slot().lock().map_err(|e| format!("serve 锁坏了：{e}"))?;
    // 双检（等待锁的功夫别人可能已经起好了）。
    if let Some(st) = guard.as_ref() {
        return Ok(ServeClient {
            base: st.base.clone(),
            agent: agent(HTTP_TIMEOUT),
        });
    }
    // 认领锁文件里的疑似孤儿（自己上次崩溃残留）：pid 活着且 /status 通就接管。
    if let Some(base) = adopt_lockfile(data_dir) {
        *guard = Some(ServeState { base: base.clone(), owned: None });
        return Ok(ServeClient {
            base,
            agent: agent(HTTP_TIMEOUT),
        });
    }
    // 起新的：找空端口 → 隐藏起进程 → 等 /status 就绪（进程先死就快速失败）。
    let port = free_port().ok_or_else(|| "8087-8099 全被占了起不了 bw serve".to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["serve", "--hostname", "127.0.0.1", "--port", &port.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("bw serve 起不来：{e}"))?;
    let base = format!("http://127.0.0.1:{port}");
    if let Err(e) = wait_ready(&base, &mut child) {
        // 进程退出（多半是未登录态）：记账 + 收尸，下次直接走 CLI 不再白等。
        let _ = child.kill();
        let _ = child.wait();
        SPAWN_DEAD.store(true, std::sync::atomic::Ordering::SeqCst);
        return Err(e);
    }
    SPAWN_DEAD.store(false, std::sync::atomic::Ordering::SeqCst);
    write_lockfile(data_dir, port);
    *guard = Some(ServeState {
        base: base.clone(),
        owned: Some(child),
    });
    Ok(ServeClient {
        base,
        agent: agent(HTTP_TIMEOUT),
    })
}

/// 退出时调：杀掉自己起的 daemon（认领/自起的都杀；别人起的不碰因为我们从不蹭）。
pub fn shutdown() {
    let mut guard = match slot().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if let Some(mut st) = guard.take() {
        if let Some(mut child) = st.owned.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 轻量 HTTP 客户端（base 固定；agent 可 Clone；阻塞调用方保证是后台线程或 fast 版）。
#[derive(Clone)]
pub struct ServeClient {
    base: String,
    agent: ureq::Agent,
}

impl ServeClient {
    fn get(&self, path: &str) -> Result<(bool, serde_json::Value, String), String> {
        self.finish(self.agent.get(&format!("{}{path}", self.base)).call())
    }

    fn get_search(&self, q: &str) -> Result<(bool, serde_json::Value, String), String> {
        let mut req = self.agent.get(&format!("{}/list/object/items", self.base));
        if !q.trim().is_empty() {
            req = req.query("search", q.trim());
        }
        self.finish(req.call())
    }

    fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<(bool, serde_json::Value, String), String> {
        let text = serde_json::to_string(body).map_err(|e| format!("serve 请求组包失败：{e}"))?;
        let res = self
            .agent
            .post(&format!("{}{path}", self.base))
            .header("Content-Type", "application/json")
            .send(&text[..]);
        self.finish(res)
    }

    fn finish(
        &self,
        res: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<(bool, serde_json::Value, String), String> {
        let mut resp = res.map_err(|e| format!("serve 调不通：{e}"))?;
        let text = resp
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()
            .map_err(|e| format!("serve 回包读不出：{e}"))?;
        envelope(&text)
    }

    /// `bw status` 三段（server_url, user_email, status）。
    /// 注意：serve 把 CLI 的裸 JSON 又包了一层 `{"object":"template","template":{…}}`，
    /// 先拆 template 再走 CLI 同款解析（两种形状都认）。
    pub fn status_info(&self) -> Result<(String, String, String), String> {
        let (ok, data, msg) = self.get("/status")?;
        if !ok {
            return Err(msg);
        }
        let inner = data.get("template").unwrap_or(&data);
        let text =
            serde_json::to_string(inner).map_err(|e| format!("serve status 非 JSON：{e}"))?;
        parse_status(&text).ok_or_else(|| "serve status 解析失败".to_string())
    }

    /// 解锁（daemon 侧持有会话；成功不用回传 key）。
    pub fn unlock(&self, password: &str) -> Result<(), String> {
        let (ok, _, msg) = self.post_json("/unlock", &serde_json::json!({ "password": password }))?;
        if ok {
            Ok(())
        } else {
            Err(msg)
        }
    }

    /// 锁定（已锁也 200 幂等）。
    pub fn lock(&self) -> Result<String, String> {
        let (ok, data, msg) = self.post_empty_raw("/lock")?;
        if ok {
            Ok(data
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("vault 已锁定")
                .to_string())
        } else {
            Err(msg)
        }
    }

    /// 同步（locked 下也 200 直接过）。
    pub fn sync(&self) -> Result<(), String> {
        let (ok, _, msg) = self.post_empty_raw("/sync")?;
        if ok {
            Ok(())
        } else {
            Err(msg)
        }
    }

    /// 空 POST（lock/sync 用）。
    fn post_empty_raw(
        &self,
        path: &str,
    ) -> Result<(bool, serde_json::Value, String), String> {
        let req = self.agent.post(&format!("{}{path}", self.base));
        self.finish(req.send_empty())
    }

    /// 列表（元数据；秘密永不进列表与 CLI 端 search 同口径）。
    /// data 形状兼容：裸数组 / {data:[…]} / template 包裹。
    pub fn list(&self, query: &str) -> Result<Vec<VaultItem>, String> {
        let (ok, data, msg) = self.get_search(query)?;
        if !ok {
            return Err(msg);
        }
        let arr = [data.get("template"), Some(&data)]
            .into_iter()
            .flatten()
            .find_map(|v| {
                if v.is_array() {
                    Some(v)
                } else {
                    v.get("data").filter(|d| d.is_array())
                }
            });
        let Some(arr) = arr else {
            return Err("serve list 形状不对".to_string());
        };
        let text = serde_json::to_string(arr).map_err(|e| format!("serve list 非 JSON：{e}"))?;
        Ok(parse_list(&text))
    }

    /// 取单项秘密（password/username；`--raw` 口径纯值）。
    /// 注意 serve 的形状：`/object/{password,username}/{id}` 回的是
    /// `data: {object:"string", data:"<纯值>"}`——纯值直接挂在 data.data 上，
    /// 不是 CLI 全对象的 data.data.raw。两种都认。
    /// TOTP serve 没暴露（404）：调用方走 CLI 回落（vault_secret 已做）。
    pub fn get_raw(&self, field: &str, id: &str) -> Result<String, String> {
        let (ok, data, msg) = self.get(&format!("/object/{field}/{id}"))?;
        if !ok {
            return Err(msg);
        }
        let raw = extract_secret(&data).unwrap_or_default();
        if raw.is_empty() {
            return Err(format!("{field} 为空"));
        }
        Ok(raw)
    }
}

/// 成功 envelope 里捞纯值（/object/{password,username}/{id} 实测形状）：
/// - `data: {object:"string", data:"<纯值>"}`（当前 bw 版本，纯值直接挂 data 下）；
/// - `data: {…, data:{raw:"…"}}`（全对象形态）；
/// - 裸 `data: {raw:"…"}` / template 包裹兼容。
/// 找不到返回 None（调用方回落 CLI，不吞错）。
fn extract_secret(data: &serde_json::Value) -> Option<String> {
    let payload = data.get("template").unwrap_or(data);
    let inner = payload.get("data").unwrap_or(payload);
    match inner {
        serde_json::Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        o => o
            .get("raw")
            .and_then(|r| r.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    }
}

/// serve envelope：{"success":bool,"data":…,"message":…}。
fn envelope(body: &str) -> Result<(bool, serde_json::Value, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("serve 回包非 JSON：{e}"))?;
    let success = v.get("success").and_then(|b| b.as_bool()).unwrap_or(false);
    let msg = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
    Ok((success, data, msg))
}

/// 找空端口（connect 被拒即空；localhost 毫秒级）。
fn free_port() -> Option<u16> {
    (PORT_START..=PORT_END).find(|p| {
        std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{p}").parse().expect("端口拼接"),
            Duration::from_millis(150),
        )
        .is_err()
    })
}

/// 等 /status 就绪（locked 也算就绪；只要求进程活着且回合法 envelope）。
/// **进程先死就快速失败**：OSS 构建 `bw serve` 在未登录态启动即退
/// （2026-11-20 实测 `You are not logged in.`）——不盯子进程就是干等 30 秒，
/// 探活与登录全链被拖死。
fn wait_ready(base: &str, child: &mut std::process::Child) -> Result<(), String> {
    let agent = agent(Duration::from_millis(800));
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            // 启动即退：原因给到调用方（具体输出拿不到——stderr 已 null，
            // 未登录态是已知主因），调用方记账后走 CLI 回落。
            Ok(Some(_)) => return Err("bw serve 启动即退出（未登录态不支持 serve）".into()),
            Ok(None) => {}
            Err(e) => return Err(format!("探测 bw serve 状态失败：{e}")),
        }
        match agent.get(&format!("{base}/status")).call() {
            Ok(mut resp) => {
                let ok = resp
                    .body_mut()
                    .with_config()
                    .limit(8192)
                    .read_to_string()
                    .ok()
                    .and_then(|t| envelope(&t).ok())
                    .is_some();
                if ok {
                    return Ok(());
                }
            }
            Err(_) => {}
        }
        if t0.elapsed() >= BOOT_TIMEOUT {
            return Err("bw serve 30 秒还没就绪".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn lockfile_path(data_dir: &Path) -> PathBuf {
    data_dir.join("bw_serve.json")
}

fn write_lockfile(data_dir: &Path, port: u16) {
    let _ = std::fs::create_dir_all(data_dir);
    let body = serde_json::json!({ "port": port, "pid": std::process::id() });
    let _ = std::fs::write(lockfile_path(data_dir), body.to_string());
}

/// 认领孤儿：锁文件 pid 还活着且 /status 通 → 当自己的用（退出时负责杀）。
fn adopt_lockfile(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(lockfile_path(data_dir)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let port = v.get("port")?.as_u64()? as u16;
    let pid = v.get("pid")?.as_u64()? as u32;
    if !pid_alive(pid) {
        return None;
    }
    let base = format!("http://127.0.0.1:{port}");
    let agent = agent(Duration::from_millis(800));
    let ok = agent
        .get(&format!("{base}/status"))
        .call()
        .ok()
        .and_then(|mut resp| {
            resp.body_mut()
                .with_config()
                .limit(8192)
                .read_to_string()
                .ok()
        })
        .and_then(|t| envelope(&t).ok())
        .is_some();
    if ok {
        Some(base)
    } else {
        None
    }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .creation_flags(0x0800_0000)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains(&pid.to_string())
        })
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}


#[cfg(test)]
mod live_tests {
    use super::*;

    /// 真 bw 二进制路径（dev 布局优先，PATH 兜底；都没有就跳过）。
    fn real_bw() -> Option<PathBuf> {
        let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/Data/bin").join(
            if cfg!(windows) {
                "bw.exe"
            } else {
                "bw"
            },
        );
        if dev.is_file() {
            return Some(dev);
        }
        super::super::Vault::detect()
    }

    /// 活体：真 daemon 全链路（status/list/lock/sync/unlock错码形状）。
    /// 跑法：cargo test -p clipx-vault -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_serve_end_to_end() {
        // panic 也杀 daemon 清目录：上次 8087 孤儿就是失败那次漏的。
        struct Guard(std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                shutdown();
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let Some(exe) = real_bw() else {
            println!("无 bw，跳过");
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "clipx-serve-live-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let _guard = Guard(dir.clone());
        let c = ensure_client(&dir, &exe).expect("daemon 应能起来");
        // status 三段（锁态以 daemon 为准）。
        let (server, email, status) = c.status_info().expect("status 应通");
        println!("status: {status} {server} {email}");
        assert!(!status.is_empty());
        // 锁定态 list 直接拒（英文短句原样上浮，给 zh 映射）。
        let err = c.list("github").expect_err("锁定态 list 应拒");
        println!("locked list err: {err}");
        // lock 幂等成功；sync 锁定态也 200。
        c.lock().expect("lock 应幂等成功");
        c.sync().expect("sync 锁定态也应 200");
        // 错主密码：KDF 跑完才拒（约 0.5s）， shaping 验证。
        let err = c.unlock("wrong-dummy-pw").expect_err("错密码应拒");
        println!("wrong unlock err: {err}");
        assert!(!err.is_empty());
    }

    #[test]
    fn extract_secret_shapes() {
        // 真机形状：/object/password/{id} → data.data 直接是纯值字符串。
        let v: serde_json::Value = serde_json::json!({"object": "string", "data": "pw123"});
        assert_eq!(extract_secret(&v).as_deref(), Some("pw123"));
        // 全对象形态。
        let v: serde_json::Value =
            serde_json::json!({"object": "item", "data": {"raw": "u@example.com"}});
        assert_eq!(extract_secret(&v).as_deref(), Some("u@example.com"));
        // template 包裹。
        let v: serde_json::Value =
            serde_json::json!({"object": "template", "template": {"object": "string", "data": "s3cr3t"}});
        assert_eq!(extract_secret(&v).as_deref(), Some("s3cr3t"));
        // 空值一律 None（调用方回落 CLI，不把空秘密当成功）。
        let v: serde_json::Value = serde_json::json!({"object": "string", "data": ""});
        assert!(extract_secret(&v).is_none());
        let v: serde_json::Value = serde_json::json!({"object": "item", "data": {}});
        assert!(extract_secret(&v).is_none());
    }
}
