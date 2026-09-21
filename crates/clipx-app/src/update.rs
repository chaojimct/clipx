//! 自动更新：GitHub Releases 检查 → 下载 → 安装 → 重启。
//!
//! - 检查：启动约 45s 静默查 `releases/latest`（也可托盘手动触发）
//! - 资产：Windows `clipx-<v>-setup.exe`（Inno 静默安装后自动重启）；
//!   macOS `clipx-<v>-macos.dmg`（下载后打开，拖入 Applications）；
//!   Linux `clipx_<v>_amd64.deb`（xdg-open 交给软件中心）/ 便携 tar.gz
//! - 网络：不引 HTTP 栈，复用系统工具（Windows PowerShell / macOS+Linux curl），
//!   与既有静默检查一致（避免为更新检查引入常驻依赖）
//! - 安装模式限定：Windows 仅在 exe 位于安装目录（%LocalAppData%\clipx）时静默更新；
//!   便携模式（Data/ 与 exe 同级）只提示，避免数据目录分裂

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::logic::AppEvt;

const API: &str = "https://api.github.com/repos/chaojimct/clipx/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/chaojimct/clipx/releases/latest";

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct LatestRelease {
    pub tag: String,
    pub assets: Vec<Asset>,
}

/// 启动约 45s 后静默查更新（对齐 WPF）。
pub fn spawn(tx: Sender<AppEvt>, last_tag: Option<String>) {
    spawn_delayed(tx, last_tag, Duration::from_secs(45), false);
}

/// 延迟查更新。
///
/// `manual = true`（托盘「检查更新」/设置关于页）时**一律回投结果**：已是最新、
/// 检查失败都要说一声——以前这两种情况都是静默 return，用户点完只能干等，
/// 表现就是"点了没反应"。后台静默检查（`manual = false`）保持不打扰，
/// 只在真发现新版本时才发事件。
pub fn spawn_delayed(tx: Sender<AppEvt>, last_tag: Option<String>, delay: Duration, manual: bool) {
    let _ = std::thread::Builder::new()
        .name("clipx-update".into())
        .spawn(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let Some(rel) = fetch_latest() else {
                if manual {
                    let _ = tx.send(AppEvt::UpdateCheckFailed);
                }
                return;
            };
            let cur = env!("CARGO_PKG_VERSION");
            if is_newer(&rel.tag, cur) {
                if manual || last_tag.as_deref() != Some(rel.tag.as_str()) {
                    let _ = tx.send(AppEvt::UpdateAvailable(rel.tag));
                }
            } else if manual {
                let _ = tx.send(AppEvt::UpdateLatest(cur.to_string()));
            }
        });
}

/// 下载并安装（后台线程；结果经 `UpdateProgress` 回投）。
/// `tag` 用于挑选资产文件名；为空则用 latest 的 tag。
pub fn download_and_install(tx: Sender<AppEvt>, tag: Option<String>) {
    let _ = std::thread::Builder::new()
        .name("clipx-update-install".into())
        .spawn(move || {
            // 阶段文字 + 可选进度条（`None` = 该阶段没有可算的比例，提示条收起进度条，
            // 但**文字仍会更新**——用户至少知道卡在哪一步）。
            // 第二参 true = 终态：设置窗据此结束「更新中」形态、恢复按钮可点。
            let report = |text: String, progress: Option<f32>, done: bool| {
                let _ = tx.send(AppEvt::UpdateProgress {
                    text,
                    progress,
                    done,
                });
            };
            report("正在获取版本信息…".into(), None, false);
            let Some(rel) = fetch_latest() else {
                report(
                    "检查更新失败：连不上 GitHub（网络/代理问题）".into(),
                    None,
                    true,
                );
                return;
            };
            let ver = tag.unwrap_or_else(|| rel.tag.clone());
            let Some(asset) = pick_asset(&rel, &ver) else {
                report("该平台暂无可用更新包，已为你打开下载页".into(), None, true);
                let _ = open_url(RELEASES_PAGE);
                return;
            };
            let Some(dir) = download_dir() else {
                report("无法创建下载目录".into(), None, true);
                return;
            };
            let dest = dir.join(&asset.name);
            // 逐块回投：同一条提示条上原地推进，不刷屏（逻辑层按最新文案覆盖）。
            let name = asset.name.clone();
            // 借用 `report`（而不是再 clone 一个 tx）：`report` 是 Fn，可以被反复调用。
            let dl = |pct: Option<f32>, read: u64, total: Option<u64>| {
                let text = match total.filter(|t| *t > 0) {
                    Some(t) => format!(
                        "正在下载 {} · {} / {}",
                        name,
                        human_size(read),
                        human_size(t)
                    ),
                    // read == 0：非 Windows 走 curl，只能拿到百分比，别硬编个「0 B」
                    None if read > 0 => format!("正在下载 {} · {}", name, human_size(read)),
                    None => format!("正在下载 {name}"),
                };
                report(text, pct, false);
            };
            report(format!("开始下载 {}", asset.name), Some(0.0), false);
            if !download(&asset.url, &dest, &dl) {
                report(
                    "下载失败：网络中断或磁盘写入被拒，请稍后重试".into(),
                    None,
                    true,
                );
                return;
            }
            report(
                format!("下载完成（{}），正在安装…", human_size(file_len(&dest))),
                Some(1.0),
                false,
            );
            match install(&dest) {
                InstallOutcome::Restarting => {
                    // Inno 静默安装会接管：关掉本进程并在装完后自动启动新版本
                    report("正在安装新版本，clipx 将自动重启…".into(), Some(1.0), false);
                    std::thread::sleep(Duration::from_millis(300));
                    std::process::exit(0);
                }
                InstallOutcome::ManualOpen => {
                    report("安装包已打开，请按提示完成更新".into(), None, true);
                }
                InstallOutcome::Portable => {
                    report("便携模式不自动覆盖，已打开下载页".into(), None, true);
                    let _ = open_url(RELEASES_PAGE);
                }
                InstallOutcome::Failed => {
                    report("启动安装程序失败，已打开下载页".into(), None, true);
                    let _ = open_url(RELEASES_PAGE);
                }
            }
        });
}

/// 人类可读体积（进度文案用）。
fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// 语义化版本比较：`0.10.2 > 0.10.1`（缺位补 0，忽略预发布后缀）。
fn is_newer(remote: &str, current: &str) -> bool {
    let nums = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (nums(remote), nums(current));
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

/// 平台资产挑选：Windows `/setup.exe`、macOS `.dmg`、Linux `.deb`（回退 tar.gz）。
fn pick_asset(rel: &LatestRelease, version: &str) -> Option<Asset> {
    let v = version.trim_start_matches('v');
    let wanted: Vec<String> = {
        #[cfg(windows)]
        {
            vec![format!("clipx-{v}-setup.exe")]
        }
        #[cfg(target_os = "macos")]
        {
            vec![format!("clipx-{v}-macos.dmg")]
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            vec![
                format!("clipx_{v}_amd64.deb"),
                format!("clipx-{v}-linux-x64.tar.gz"),
            ]
        }
    };
    for w in &wanted {
        if let Some(a) = rel.assets.iter().find(|a| a.name == *w) {
            return Some(a.clone());
        }
    }
    // 名字不匹配（版本号写法差异）时退化为按后缀挑
    let suffix: &str = {
        #[cfg(windows)]
        {
            "-setup.exe"
        }
        #[cfg(target_os = "macos")]
        {
            "-macos.dmg"
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            "_amd64.deb"
        }
    };
    rel.assets.iter().find(|a| a.name.ends_with(suffix)).cloned()
}

fn fetch_latest() -> Option<LatestRelease> {
    let json = fetch_latest_json()?;
    parse_release(&json)
}

/// 拉取 release JSON：Windows 用 PowerShell（CREATE_NO_WINDOW 防闪窗），
/// macOS/Linux 用系统 curl。
fn fetch_latest_json() -> Option<String> {
    let out = {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "(Invoke-RestMethod -Uri '{API}' -Headers @{{'User-Agent'='clipx'}}) | ConvertTo-Json -Depth 6 -Compress"
                    ),
                ])
                .creation_flags(0x0800_0000)
                .output()
                .ok()?
        }
        #[cfg(not(windows))]
        {
            std::process::Command::new("curl")
                .args(["-sL", "-H", "User-Agent: clipx", API])
                .output()
                .ok()?
        }
    };
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn parse_release(json: &str) -> Option<LatestRelease> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?.trim().to_string();
    let assets = v
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("browser_download_url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(LatestRelease { tag, assets })
}

fn download_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("clipx-update");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// 下载安装包，并逐块回投进度。
///
/// 为什么不用 `Invoke-WebRequest`：它到整个文件下完前一个字节都不吐，做不了进度条。
/// 这里改用 `HttpClient` + `ResponseHeadersRead` 手工分块读，每块把 `已读/总长`
/// 打到 stdout，Rust 侧逐行解析后回投（`report(百分比, 已读, 总长)`）。
/// 服务端不给 `Content-Length` 时 `total = None`，此时只报已读字节、进度条走不确定态。
fn download(url: &str, dest: &Path, report: &dyn Fn(Option<f32>, u64, Option<u64>)) -> bool {
    let _ = std::fs::remove_file(dest);

    #[cfg(windows)]
    {
        use std::io::{BufRead, BufReader};
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let script = format!(
            "$ErrorActionPreference='Stop'\n\
             [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12\n\
             $c=New-Object System.Net.Http.HttpClient\n\
             $c.Timeout=[TimeSpan]::FromMinutes(30)\n\
             $c.DefaultRequestHeaders.Add('User-Agent','clipx')\n\
             $r=$c.GetAsync({url},[System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()\n\
             $r.EnsureSuccessStatusCode() | Out-Null\n\
             $total=$r.Content.Headers.ContentLength\n\
             $s=$r.Content.ReadAsStreamAsync().GetAwaiter().GetResult()\n\
             $f=[System.IO.File]::Create({dest})\n\
             $buf=New-Object byte[] 131072\n\
             $read=0\n\
             while(($n=$s.Read($buf,0,$buf.Length)) -gt 0){{ $f.Write($buf,0,$n); $read+=$n; [Console]::Out.WriteLine(\"$read/$total\"); [Console]::Out.Flush() }}\n\
             $f.Close(); $s.Close()",
            url = ps_literal(url),
            dest = ps_literal(&dest.to_string_lossy()),
        );
        let Ok(mut child) = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
        else {
            return false;
        };
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(std::result::Result::ok) {
                if let Some((read, total)) = parse_progress_line(&line) {
                    let pct = total.filter(|t| *t > 0).map(|t| read as f32 / t as f32);
                    report(pct, read, total);
                }
            }
        }
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        ok && dest.is_file()
    }

    #[cfg(not(windows))]
    {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let Ok(mut child) = Command::new("curl")
            .args(["-fL", "--progress-bar", "-o"])
            .arg(dest)
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        else {
            return false;
        };
        if let Some(err) = child.stderr.take() {
            // curl 的进度条用 \r 原地刷新，按 \r 切段解析百分比。
            let mut r = BufReader::new(err);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\r', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if buf.len() > 4096 {
                    continue; // 异常输出，丢掉不解析
                }
                if let Some(p) = parse_percent(&String::from_utf8_lossy(&buf)) {
                    report(Some(p), 0, None);
                }
            }
        }
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        ok && dest.is_file()
    }
}

/// PowerShell 单引号字符串字面量（内部单引号翻倍转义）。
#[cfg(windows)]
fn ps_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 解析下载脚本打出的一行 `已读/总长`（总长可能为空 = 服务端没给 Content-Length）。
#[cfg(windows)]
fn parse_progress_line(line: &str) -> Option<(u64, Option<u64>)> {
    let (a, b) = line.trim().split_once('/')?;
    let read = a.trim().parse::<u64>().ok()?;
    Some((read, b.trim().parse::<u64>().ok()))
}

/// 从 curl `-#` 的进度行里抠百分比（形如 `###### 45.3%`）。
#[cfg(not(windows))]
fn parse_percent(s: &str) -> Option<f32> {
    let p = s.rfind('%')?;
    let digits: String = s[..p]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits
        .chars()
        .rev()
        .collect::<String>()
        .parse::<f32>()
        .ok()
        .map(|v| v / 100.0)
}

enum InstallOutcome {
    /// Windows 安装模式：已拉起静默安装器，本进程退出后自动重启新版本
    Restarting,
    /// 已打开安装包，用户手动完成（mac dmg / Linux deb）。
    /// Windows 走静默安装分支，故本平台无构造点——平台条件所致，非死代码。
    #[allow(dead_code)]
    ManualOpen,
    /// 便携模式：不自动装（避免数据目录分裂）
    Portable,
    Failed,
}

fn install(asset_path: &Path) -> InstallOutcome {
    #[cfg(windows)]
    {
        if !running_from_install_dir() {
            return InstallOutcome::Portable;
        }
        use std::os::windows::process::CommandExt;
        let ok = std::process::Command::new(asset_path)
            .args([
                "/SILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/CLOSEAPPLICATIONS",
            ])
            .creation_flags(0x0800_0000)
            .spawn()
            .is_ok();
        if ok {
            InstallOutcome::Restarting
        } else {
            InstallOutcome::Failed
        }
    }
    #[cfg(target_os = "macos")]
    {
        // dmg 挂载后由用户拖入 Applications（未签名包需右键打开）
        if open_path_with("open", asset_path) {
            InstallOutcome::ManualOpen
        } else {
            InstallOutcome::Failed
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        if open_path_with("xdg-open", asset_path) {
            InstallOutcome::ManualOpen
        } else {
            InstallOutcome::Failed
        }
    }
}

/// Windows：exe 是否位于安装目录（`%LocalAppData%\clipx`）。
/// 便携模式（exe 同级 Data/）不自动更新，避免数据目录分裂。
#[cfg(windows)]
fn running_from_install_dir() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(parent) = exe.parent() else {
        return false;
    };
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return false;
    };
    parent
        .to_string_lossy()
        .to_lowercase()
        .starts_with(&PathBuf::from(local).join("clipx").to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
fn open_path_with(cmd: &str, path: &Path) -> bool {
    std::process::Command::new(cmd)
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 用系统默认程序打开 URL（设置「关于」页的链接、更新失败时的下载页都走这里）。
pub fn open_url(url: &str) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .creation_flags(0x0800_0000)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        open_path_with("xdg-open", Path::new(url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.10.2", "0.10.1"));
        assert!(is_newer("v0.11.0", "0.10.9"));
        assert!(!is_newer("0.10.1", "0.10.1"));
        assert!(!is_newer("0.9.9", "0.10.0"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("0.10.1-beta1", "0.10.1"));
    }

    #[test]
    fn parses_release_assets() {
        let json = serde_json::json!({
            "tag_name": "v0.10.2",
            "assets": [
                {"name": "clipx-0.10.2-setup.exe", "browser_download_url": "https://x/a.exe"},
                {"name": "clipx-0.10.2-macos.dmg", "browser_download_url": "https://x/a.dmg"}
            ]
        })
        .to_string();
        let rel = parse_release(&json).expect("parse");
        assert_eq!(rel.tag, "v0.10.2");
        assert_eq!(rel.assets.len(), 2);
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MB");
        assert_eq!(human_size(9_560_000), "9.1 MB");
    }

    /// 下载脚本每块打一行 `已读/总长`；服务端不给 Content-Length 时总长为空。
    #[cfg(windows)]
    #[test]
    fn parses_download_progress_lines() {
        assert_eq!(parse_progress_line("12345/67890"), Some((12345, Some(67890))));
        assert_eq!(parse_progress_line("12345/"), Some((12345, None)));
        assert_eq!(parse_progress_line("  7 / 10 "), Some((7, Some(10))));
        assert_eq!(parse_progress_line("no slash here"), None);
        assert_eq!(parse_progress_line("abc/10"), None);
    }

    /// curl 的 `-#` 进度行（非 Windows 路径）。
    #[cfg(not(windows))]
    #[test]
    fn parses_curl_percent() {
        let p = parse_percent("############################ 45.3%").expect("percent");
        assert!((p - 0.453).abs() < 1e-6, "got {p}");
        assert!(parse_percent("no percent").is_none());
    }

    #[test]
    fn picks_platform_asset_by_suffix_fallback() {
        let rel = LatestRelease {
            tag: "v0.10.2".into(),
            assets: vec![Asset {
                name: "whatever-name-app.exe".into(),
                url: "https://x/a".into(),
            }],
        };
        let picked = pick_asset(&rel, "0.10.2");
        #[cfg(windows)]
        assert!(picked.is_none(), "非 -setup.exe 后缀不应误选");
        #[cfg(not(windows))]
        assert!(picked.is_none());
    }
}
