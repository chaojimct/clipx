//! 全局策略：排除应用名单 + 面板主键（WPF `ExclusionApps` / `PanelModifierKey`）。
//!
//! - 热键线程每次触发前查 `is_foreground_excluded()`（前台属名单则吞掉）
//! - 处理器入库前再查一次（排除应用复制不落库）+ 暂停采集开关
//! - 键盘钩子读 `panel_vk()` 识别 `m+N / m+Tab`（主键可配 Ctrl/Alt/Win/CapsLock）

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static EXCLUSIONS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static PANEL_VK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x11);
static CAPTURE_PAUSED: AtomicBool = AtomicBool::new(false);
static OCR_ENABLED: AtomicBool = AtomicBool::new(true);

fn exclusions() -> &'static Mutex<Vec<String>> {
    EXCLUSIONS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 更新排除名单（设置保存/启动时调用；存小写无 `.exe` 形态）。
pub fn set_exclusions(apps: &[String]) {
    let norm: Vec<String> = apps
        .iter()
        .map(|a| {
            let t = a.trim().to_lowercase();
            t.strip_suffix(".exe").unwrap_or(&t).to_string()
        })
        .filter(|a| !a.is_empty())
        .collect();
    *exclusions().lock().unwrap() = norm;
}

/// 前台窗口是否属于排除名单。
pub fn is_foreground_excluded() -> bool {
    let list = exclusions().lock().unwrap();
    if list.is_empty() {
        return false;
    }
    let exe = foreground_exe_base();
    !exe.is_empty() && list.iter().any(|a| a == &exe)
}

/// 前台是否密码管理器（M1 安全网关）：从密码管理器窗口复制出来的内容
/// （主密码、条目、TOTP）**永不入库**——剪贴板管理器的头号安全红线
/// （Ditto 因缺这道门被长期诟病，CopyQ 用格式门 + 窗口门双保险）。
///
/// 与用户自配的 `exclusion_apps` 独立：内置表覆盖主流密码管理器，
/// 用户仍可用排除名单补漏。非 Windows 上 `foreground_exe_base()` 为空，
/// 本门恒返回 false（mac/Linux 的格式门随 M6/M7 落地）。
pub fn is_password_manager_foreground() -> bool {
    let exe = foreground_exe_base();
    !exe.is_empty() && is_password_manager_stem(&exe)
}

/// 纯逻辑：exe stem（小写、无后缀）是否密码管理器。可单测。
pub fn is_password_manager_stem(stem: &str) -> bool {
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "bitwarden"
            | "1password"
            | "keepass"
            | "keepass2"
            | "keepassxc"
            | "keepassx"
            | "lastpass"
            | "dashlane"
            | "nordpass"
            | "enpass"
            | "roboform"
            | "keeper"
            | "protonpass"
    )
}

/// 面板主键虚键码（WPF `PanelModifierKey`：Ctrl=0x11/Alt=0x12/Win=0x5B/Caps=0x14）。
pub fn set_panel_key(name: &str) {
    let vk = match name {
        "Alt" => 0x12,
        "Win" => 0x5B,
        "CapsLock" => 0x14,
        _ => 0x11,
    };
    PANEL_VK.store(vk, std::sync::atomic::Ordering::SeqCst);
}

pub fn panel_vk() -> u32 {
    PANEL_VK.load(Ordering::SeqCst)
}

/// 托盘「暂停采集」：处理器跳过入库，监听线程仍消费事件以免撑满通道。
pub fn set_capture_paused(paused: bool) {
    CAPTURE_PAUSED.store(paused, Ordering::SeqCst);
}

pub fn is_capture_paused() -> bool {
    CAPTURE_PAUSED.load(Ordering::SeqCst)
}

pub fn toggle_capture_paused() -> bool {
    let next = !is_capture_paused();
    set_capture_paused(next);
    next
}

/// OCR 热开关：队列常驻，仅门控入队（设置保存即时生效）。
pub fn set_ocr_enabled(enabled: bool) {
    OCR_ENABLED.store(enabled, Ordering::SeqCst);
}

pub fn is_ocr_enabled() -> bool {
    OCR_ENABLED.load(Ordering::SeqCst)
}

/// 替换 Win+V：只拦截热键（`keyboard_hook` 低级钩子吞掉按键），不动系统剪贴板历史开关。
/// 这里曾经写 `EnableClipboardHistory=0`、退出写回 1，导致三个问题不再这么做：
/// 1. 关历史 = Windows 按隐私语义**清空已存 Win+V 记录**（退出即空）；
/// 2. 注册表切换要 cbdhsvc 重载（体感“原生剪贴板要重启才回来”）；
/// 3. 与启动回补（读系统历史 `GetHistoryItemsAsync`）互斥：关了永远读空。
/// 系统历史保持开启，两边共存（剪贴板监听是广播链多家可并存），钩子照样吞键，
/// 原生面板不会弹出来所以没有冲突。
pub fn apply_win_v_replace(_replace: bool) {
    // 热键替换只走 keyboard_hook::set_replace_win_v 的钩子，此处不再动注册表。
}

/// 退出恢复：不再动注册表（无须恢复；旧版本写 0 的由 `system_history` 一次性治愈）。
pub fn restore_win_v_history() {}

/// 前台进程基名（小写、无 .exe）；非 Windows 或取失败为空串。
pub fn foreground_app() -> String {
    foreground_exe_base()
}

#[cfg(windows)]
fn foreground_exe_base() -> String {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return String::new();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return String::new();
        }
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            windows::Win32::System::Threading::PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if ok.is_err() {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        let base = full.rsplit(['\\', '/']).next().unwrap_or("");
        let base = base
            .strip_suffix(".exe")
            .or_else(|| base.strip_suffix(".EXE"))
            .unwrap_or(base);
        base.to_lowercase()
    }
}

#[cfg(not(windows))]
fn foreground_exe_base() -> String {
    String::new()
}

/// 顶层进程名（不带扩展名，小写去重排序，供排除应用添加器用）。
pub fn top_process_names() -> Vec<String> {
    #[cfg(windows)]
    {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let mut out = std::collections::BTreeSet::new();
        let self_pid = std::process::id();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return Vec::new();
            };
            let mut pe = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snap, &mut pe).is_ok() {
                loop {
                    if pe.th32ProcessID != self_pid && pe.th32ProcessID > 4 {
                        let end = pe
                            .szExeFile
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(pe.szExeFile.len());
                        let name =
                            String::from_utf16_lossy(&pe.szExeFile[..end]).to_lowercase();
                        let base = name
                            .strip_suffix(".exe")
                            .unwrap_or(&name)
                            .to_string();
                        if !base.is_empty() {
                            out.insert(base);
                        }
                    }
                    if Process32NextW(snap, &mut pe).is_err() {
                        break;
                    }
                }
            }
            let _ = windows::Win32::Foundation::CloseHandle(snap);
        }
        out.into_iter().take(24).collect()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_manager_stems() {
        for exe in [
            "bitwarden",
            "Bitwarden",
            "1password",
            "1Password.exe",
            "KeePassXC",
            "keepass2",
            "LastPass",
            "Dashlane",
            "NordPass",
            "Enpass",
            "RoboForm",
            "Keeper",
            "ProtonPass",
        ] {
            // 入参可能是各种形态：统一归一化后再判（与 set_exclusions 同归一）。
            let t = exe.trim().to_lowercase();
            let stem = t.strip_suffix(".exe").unwrap_or(&t);
            assert!(
                is_password_manager_stem(stem),
                "{exe} 应命中密码管理器表"
            );
        }
        for exe in ["", "chrome", "wechat", "explorer", "clipx", "notepad"] {
            assert!(
                !is_password_manager_stem(exe),
                "{exe} 不应命中密码管理器表"
            );
        }
    }
}
