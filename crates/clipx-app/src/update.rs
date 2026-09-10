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
    spawn_delayed(tx, last_tag, Duration::from_secs(45));
}

pub fn spawn_delayed(tx: Sender<AppEvt>, last_tag: Option<String>, delay: Duration) {
    let _ = std::thread::Builder::new()
        .name("clipx-update".into())
        .spawn(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            if let Some(rel) = fetch_latest() {
                let cur = env!("CARGO_PKG_VERSION");
                if is_newer(&rel.tag, cur) && last_tag.as_deref() != Some(rel.tag.as_str()) {
                    let _ = tx.send(AppEvt::UpdateAvailable(rel.tag));
                }
            }
        });
}

/// 下载并安装（后台线程；结果经 `UpdateProgress` 回投）。
/// `tag` 用于挑选资产文件名；为空则用 latest 的 tag。
pub fn download_and_install(tx: Sender<AppEvt>, tag: Option<String>) {
    let _ = std::thread::Builder::new()
        .name("clipx-update-install".into())
        .spawn(move || {
            let progress = |msg: &str| {
                let _ = tx.send(AppEvt::UpdateProgress(msg.to_string()));
            };
            let Some(rel) = fetch_latest() else {
                progress("检查更新失败（网络不可达）");
                return;
            };
            let ver = tag.unwrap_or_else(|| rel.tag.clone());
            let Some(asset) = pick_asset(&rel, &ver) else {
                progress("该平台暂无可用更新包");
                let _ = open_url(RELEASES_PAGE);
                return;
            };
            let Some(dir) = download_dir() else {
                progress("无法创建下载目录");
                return;
            };
            let dest = dir.join(&asset.name);
            progress(&format!("正在下载 {} …", asset.name));
            if !download(&asset.url, &dest) {
                progress("下载失败，请稍后重试");
                return;
            }
            progress("下载完成，正在安装 …");
            match install(&dest) {
                InstallOutcome::Restarting => {
                    // Inno 静默安装会接管：关掉本进程并在装完后自动启动新版本
                    progress("正在安装新版本，clipx 将自动重启 …");
                    std::thread::sleep(Duration::from_millis(300));
                    std::process::exit(0);
                }
                InstallOutcome::ManualOpen => {
                    progress("安装包已打开，请按提示完成更新");
                }
                InstallOutcome::Portable => {
                    progress("便携模式请手动下载覆盖");
                    let _ = open_url(RELEASES_PAGE);
                }
                InstallOutcome::Failed => {
                    progress("启动安装程序失败，请手动更新");
                    let _ = open_url(RELEASES_PAGE);
                }
            }
        });
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

fn download(url: &str, dest: &Path) -> bool {
    let _ = std::fs::remove_file(dest);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Invoke-WebRequest -Uri '{url}' -OutFile '{}' -UseBasicParsing",
                    dest.display()
                ),
            ])
            .creation_flags(0x0800_0000)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && dest.is_file()
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("curl")
            .args(["-sL", "-o"])
            .arg(dest)
            .arg(url)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && dest.is_file()
    }
}

enum InstallOutcome {
    /// Windows 安装模式：已拉起静默安装器，本进程退出后自动重启新版本
    Restarting,
    /// 已打开安装包，用户手动完成（mac dmg / Linux deb）
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

fn open_url(url: &str) -> bool {
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
