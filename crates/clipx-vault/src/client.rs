//! `bw` CLI 封装：进程边界、超时、输出上限、秘密脱敏。
//!
//! 安全纪律（违反即返工）：
//! - 会话 key 只走子进程**环境** `BW_SESSION`，永不进 argv；
//! - 主密码只走单次 unlock 子进程的 `--passwordenv`（父进程不留存）；
//! - 秘密输出只以 `Zeroizing<String>` 返回，调用方用完即 drop；
//! - 错误信息不含秘密；`Debug` 脱敏；stdout 上限 1MB，超时一律 kill。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use super::model::{parse_list, parse_status, VaultItem};

/// 默认超时（list 在大 vault 上可能数秒；get 应在 1s 内）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// stdout 上限（防异常输出撑内存）。
pub const MAX_STDOUT: u64 = 1024 * 1024;
/// 传给单次 unlock 子进程的口令环境变量名（父进程环境不留存）。
pub const UNLOCK_PW_ENV: &str = "CLIPX_BW_UNLOCK_PW";
/// 会话环境变量名（`bw` 官方约定）。
pub const SESSION_ENV: &str = "BW_SESSION";

#[derive(Debug, PartialEq, Eq)]
pub enum VaultError {
    /// 找不到 `bw` 可执行文件。
    Missing(String),
    /// vault 已锁定（需要 unlock；不含任何秘密信息）。
    Locked,
    /// 未登录（需要 `bw login`）。
    Unauthenticated,
    /// 命令超时（已 kill）。
    Timeout(String),
    /// 输出解析失败。
    Parse(String),
    /// `bw` 返回非零退出（stderr 截断，不含秘密）。
    Bw(String),
    /// 启动/IO 失败。
    Io(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::Missing(e) => write!(f, "找不到 bw：{e}"),
            VaultError::Locked => write!(f, "vault 已锁定"),
            VaultError::Unauthenticated => write!(f, "bw 未登录"),
            VaultError::Timeout(e) => write!(f, "bw 超时：{e}"),
            VaultError::Parse(e) => write!(f, "bw 输出解析失败：{e}"),
            VaultError::Bw(e) => write!(f, "bw 报错：{e}"),
            VaultError::Io(e) => write!(f, "启动 bw 失败：{e}"),
        }
    }
}

impl std::error::Error for VaultError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultState {
    /// PATH 上没有 `bw`。
    Missing,
    /// 未登录或已锁定（无内存会话）。
    Locked,
    /// 内存有会话（可用）。
    Unlocked,
}

/// 只读 vault 会话。`Clone` 无意保持：会话只应存在一处（State 内）。
pub struct Vault {
    exe: PathBuf,
    session: Option<Zeroizing<String>>,
    timeout: Duration,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("exe", &self.exe)
            .field("session", &self.session.as_ref().map(|_| "***"))
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Vault {
    pub fn new(exe: impl Into<PathBuf>) -> Self {
        Self {
            exe: exe.into(),
            session: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// PATH 上找 `bw`（Windows 顺带找 `bw.exe`）。找不到则调用方走降级提示。
    pub fn detect() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            for name in ["bw", "bw.exe"] {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        None
    }

    /// 用进程环境 `BW_SESSION`（终端 `bw unlock --raw` 后导出）建立内存会话。
    /// 环境里没有则返回 None（调用方显示"已锁定"提示行）。
    pub fn open_env(exe: impl Into<PathBuf>) -> Option<Self> {
        let session: Zeroizing<String> = std::env::var(SESSION_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(Zeroizing::new)?;
        let mut v = Self::new(exe);
        v.session = Some(session);
        Some(v)
    }

    pub fn has_session(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.is_empty())
    }

    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// `bw unlock --raw --passwordenv`：口令只进子进程环境，返回会话 key。
    /// 成功则替换内存会话；失败不碰旧会话。
    pub fn unlock(&mut self, password: &str) -> Result<bool, VaultError> {
        let out = self.run(&["unlock", "--raw", "--passwordenv", UNLOCK_PW_ENV], |cmd| {
            cmd.env(UNLOCK_PW_ENV, password);
        })?;
        let key = out.trim().to_string();
        if key.is_empty() {
            return Ok(false);
        }
        self.session = Some(Zeroizing::new(key));
        Ok(true)
    }

    /// 丢弃内存会话（另尽力 `bw lock` 服务端侧，失败忽略）。
    pub fn lock(&mut self) {
        self.session = None;
        let _ = self.run(&["lock"], |_| {});
    }

    pub fn state(&self) -> VaultState {
        // 二进制不存在 → Missing；否则无会话就是 Locked（登录态由 unlock 解决）。
        let binary_ok =
            self.exe.is_file() || self.which_self(&self.exe).is_some();
        if !binary_ok {
            // exe 可能是裸名但不在 PATH：探活一次，spawn 失败即 Missing。
            match self.run(&["status"], |_| {}) {
                Err(VaultError::Io(_)) | Err(VaultError::Missing(_)) => {
                    return VaultState::Missing
                }
                _ => {}
            }
        }
        if !self.has_session() {
            return VaultState::Locked;
        }
        match self.run(&["status"], |_| {}) {
            Ok(json) => match parse_status(&json) {
                Some((_, _, s)) if s == "unlocked" => VaultState::Unlocked,
                _ => VaultState::Locked,
            },
            Err(_) => VaultState::Locked,
        }
    }

    pub fn sync(&self) -> Result<(), VaultError> {
        self.require_session()?;
        self.run(&["sync"], |_| {}).map(|_| ())
    }

    /// `bw list items --search` → 只要元数据（秘密永不进列表）。
    pub fn search(&self, query: &str) -> Result<Vec<VaultItem>, VaultError> {
        self.require_session()?;
        let q = query.trim();
        let args: Vec<&str> = if q.is_empty() {
            vec!["list", "items"]
        } else {
            vec!["list", "items", "--search", q]
        };
        let json = self.run(&args, |_| {})?;
        Ok(parse_list(&json))
    }

    /// 现取密码（`--raw` 纯值）。调用方用完即 drop，不得存任何结构。
    pub fn password(&self, id: &str) -> Result<Zeroizing<String>, VaultError> {
        self.get_raw("password", id)
    }

    /// 现取 TOTP（`--raw` 纯值，6 位数字）。
    pub fn totp(&self, id: &str) -> Result<Zeroizing<String>, VaultError> {
        self.get_raw("totp", id)
    }

    pub fn username(&self, id: &str) -> Result<String, VaultError> {
        self.require_session()?;
        let out = self.run(&["get", "username", id, "--raw"], |_| {})?;
        Ok(out.trim().to_string())
    }

    fn get_raw(&self, field: &str, id: &str) -> Result<Zeroizing<String>, VaultError> {
        self.require_session()?;
        let out = self.run(&["get", field, id, "--raw"], |_| {})?;
        let v = out.trim().to_string();
        if v.is_empty() {
            return Err(VaultError::Parse(format!("{field} 为空")));
        }
        Ok(Zeroizing::new(v))
    }

    fn require_session(&self) -> Result<(), VaultError> {
        if self.has_session() {
            Ok(())
        } else {
            Err(VaultError::Locked)
        }
    }

    fn which_self(&self, exe: &Path) -> Option<PathBuf> {
        let name = exe.to_string_lossy().into_owned();
        std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(&name))
                .find(|f| f.is_file())
        })
    }

    /// 跑一次 `bw`：stdin 关闭、stdout/stderr 边跑边读（防管道撑满死锁）、
    /// 输出上限、超时 kill、stderr 截断。
    /// 会话经环境传入；argv 里永不出现秘密（只有子命令/id/查询词）。
    fn run(
        &self,
        args: &[&str],
        with: impl FnOnce(&mut Command),
    ) -> Result<String, VaultError> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(s) = self.session.as_ref() {
            cmd.env(SESSION_ENV, s.as_str());
        }
        with(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                VaultError::Missing(self.exe.to_string_lossy().into_owned())
            } else {
                VaultError::Io(e.to_string())
            }
        })?;
        // 先起读线程再等退出：输出超 64KB 管道缓冲时，不读就等必然死锁。
        let out_h = match child.stdout.take() {
            Some(so) => Some(
                std::thread::Builder::new()
                    .name("clipx-vault-read".into())
                    .spawn(move || {
                        let mut buf = Vec::new();
                        let limited = so.take(MAX_STDOUT + 1).read_to_end(&mut buf);
                        (limited, buf)
                    })
                    .map_err(|e| VaultError::Io(e.to_string()))?,
            ),
            None => None,
        };
        let err_h = match child.stderr.take() {
            Some(se) => Some(
                std::thread::Builder::new()
                    .name("clipx-vault-readerr".into())
                    .spawn(move || {
                        let mut buf = Vec::new();
                        let _ = se.take(4096).read_to_end(&mut buf);
                        buf
                    })
                    .map_err(|e| VaultError::Io(e.to_string()))?,
            ),
            None => None,
        };
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait().map_err(|e| VaultError::Io(e.to_string()))? {
                Some(status) => break status,
                None => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(VaultError::Timeout(format!("{}s", self.timeout.as_secs())));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        let out = match out_h {
            Some(h) => h.join().map_err(|_| VaultError::Io("读取输出线程崩溃".into()))?,
            None => (Ok(0), Vec::new()),
        };
        let err = err_h
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let (limited, buf) = out;
        if limited.is_err() || buf.len() as u64 > MAX_STDOUT {
            return Err(VaultError::Parse("输出超限或不可读".into()));
        }
        let text = String::from_utf8(buf).map_err(|e| VaultError::Parse(e.to_string()))?;
        if !status.success() {
            let emsg = first_line(&String::from_utf8_lossy(&err), 200);
            if emsg.to_ascii_lowercase().contains("locked")
                || emsg.to_ascii_lowercase().contains("not unlocked")
            {
                return Err(VaultError::Locked);
            }
            if emsg.to_ascii_lowercase().contains("not logged in")
                || emsg.to_ascii_lowercase().contains("unauthenticated")
            {
                return Err(VaultError::Unauthenticated);
            }
            return Err(VaultError::Bw(emsg));
        }
        Ok(text)
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_env_name_is_official() {
        assert_eq!(SESSION_ENV, "BW_SESSION");
    }

    #[test]
    fn debug_redacts_session() {
        let mut v = Vault::new("bw");
        v.session = Some(Zeroizing::new("supersecret".to_string()));
        let dbg = format!("{v:?}");
        assert!(dbg.contains("***"));
        assert!(!dbg.contains("supersecret"));
    }

    #[test]
    fn errors_carry_no_secrets() {
        // 错误展示面审计：构造的所有错误都不应含秘密占位。
        for e in [
            VaultError::Missing("x".into()),
            VaultError::Locked,
            VaultError::Unauthenticated,
            VaultError::Timeout("t".into()),
            VaultError::Parse("p".into()),
            VaultError::Bw("b".into()),
            VaultError::Io("i".into()),
        ] {
            let s = e.to_string();
            assert!(!s.contains("supersecret"), "{s}");
        }
    }

    #[test]
    fn state_without_binary_is_missing_or_locked() {
        // 无 bw、无会话：绝不 panic，只降级。
        let v = Vault::new("definitely-not-a-real-binary-xyz");
        assert!(!v.has_session());
        // run 必然 Io/Missing → state 不应是 Unlocked。
        assert_ne!(v.state(), VaultState::Unlocked);
    }

    /// 活体（需本机 `bw` + 已登录 vault）：默认忽略。
    /// 跑法：cargo test -p clipx-vault -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_status_probe() {
        let Some(exe) = Vault::detect() else {
            println!("本机无 bw，跳过");
            return;
        };
        let v = Vault::new(exe);
        println!("state without session: {:?}", v.state());
    }

    /// stub bw 的 Rust 源码（无依赖）：按 argv 吐固定 JSON，
    /// 验证整条链路（spawn/管道/超时/会话环境/上限）。
    const STUB_SRC: &str = r##"
use std::io::Write;
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let need_session = matches!(a.first().map(|s| s.as_str()), Some("list") | Some("get"));
    if need_session && std::env::var("BW_SESSION").map(|s| s.is_empty()).unwrap_or(true) {
        eprintln!("Vault is locked.");
        std::process::exit(1);
    }
    match a.first().map(|s| s.as_str()) {
        Some("status") => {
            print!("{}", r#"{"serverUrl":"https://x","userEmail":"a@b","status":"unlocked"}"#);
        }
        Some("list") => {
            print!("{}", r#"[{"object":"item","id":"stub-1","name":"stub-github","login":{"username":"u","uris":[{"uri":"https://g.h"}]}},{"object":"item","id":"stub-2","name":"stub-mail","login":{}}]"#);
        }
        Some("get") => {
            let field = a.get(1).cloned().unwrap_or_default();
            let id = a.get(2).cloned().unwrap_or_default();
            if id == "empty" {
                print!("");
            } else {
                print!("SECRET-{field}-{id}");
            }
        }
        Some("unlock") => {
            print!("session-key-123");
        }
        Some("sync") => {}
        Some("sleep") => std::thread::sleep(std::time::Duration::from_secs(60)),
        Some("big") => {
            let _ = std::io::stdout().write_all(&vec![b'z'; 2 * 1024 * 1024]);
        }
        _ => {
            eprintln!("unknown");
            std::process::exit(2);
        }
    }
}
"##;

    /// 现场编译 stub bw.exe（rustc 在 dev/CI 全平台可用；没有就带原因跳过）。
    fn build_stub() -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("clipx-vault-stub-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("stub.rs");
        let exe = dir.join(if cfg!(windows) { "bw.exe" } else { "bw" });
        if std::fs::write(&src, STUB_SRC).is_err() {
            return None;
        }
        let r = std::process::Command::new("rustc")
            .arg("--edition=2021")
            .arg("-O")
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .output()
            .ok()?;
        if !r.status.success() || !exe.is_file() {
            eprintln!("跳过 stub 测试：rustc 不可用");
            return None;
        }
        Some(exe)
    }

    #[test]
    fn stub_end_to_end_status_search_get() {
        let Some(exe) = build_stub() else { return };
        let mut v = Vault::new(&exe);
        v.timeout = Duration::from_secs(10);
        // 无会话：search 必须 Locked（且不 spawn 读管道）。
        assert!(matches!(v.search("x"), Err(VaultError::Locked)));
        // unlock：口令只进子进程环境，返回会话。
        assert_eq!(v.unlock("master-pw"), Ok(true));
        assert!(v.has_session());
        assert_eq!(v.state(), VaultState::Unlocked);
        // search：只要元数据。
        let items = v.search("stub").expect("stub search");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "stub-1");
        assert_eq!(items[0].username, "u");
        // get：现取秘密，Zeroizing 到手即有值。
        let pw = v.password("stub-1").expect("stub get");
        assert_eq!(&*pw, "SECRET-password-stub-1");
        // 空值 → Parse 错误（不返回空秘密）。
        assert!(matches!(v.password("empty"), Err(VaultError::Parse(_))));
        // lock：会话清零后再查即 Locked。
        v.lock();
        assert!(!v.has_session());
        assert!(matches!(v.search("x"), Err(VaultError::Locked)));
    }

    #[test]
    fn stub_timeout_kills_and_big_output_rejected() {
        let Some(exe) = build_stub() else { return };
        let mut v = Vault::new(&exe);
        // 超时：sleep 60s 的子进程必须被 kill（给 1s 预算验证 kill 路径）。
        v.timeout = Duration::from_millis(800);
        let t0 = Instant::now();
        let r = v.run(&["sleep"], |_| {});
        assert!(matches!(r, Err(VaultError::Timeout(_))), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(20));
        // 超限输出：2MB > 1MB 上限 → Parse 错误（管道死锁修好了才能走到这里）。
        v.timeout = Duration::from_secs(10);
        let r = v.run(&["big"], |_| {});
        assert!(matches!(r, Err(VaultError::Parse(_))), "{r:?}");
    }
}
