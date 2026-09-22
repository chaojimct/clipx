//! 老版 WPF ClipboardX 的检测与自启项停用（迁移「丝滑」的收尾环节）。
//!
//! 背景：clipx 首启会**自动导入**老版历史（`wpf_import.rs`），但老版程序本身
//! 还在自启、还在写自己的库——两个剪贴板监听并行、热键相撞、数据分叉。
//! 老用户从老版更新通道迁过来后，需要有人告诉他「老版可以退休了」，并且
//! 能安全地停掉它。
//!
//! **本模块只停自启项，绝不碰老版的程序目录与数据目录。**
//! 老版卸载向导会询问「是否同时删除配置与历史记录」，选「是」会递归删掉
//! `%LocalAppData%\ClipboardX`（历史库就在这里）——所以卸载这件事交给用户
//! 走老版自己的卸载器，我们只把「先导入、再卸载、选『否』」讲清楚。
//!
//! 事实依据（2026-09-22 从 `../clipboard` 源码核实）：
//! - 自启：`HKCU\...\Run` 值名 `ClipboardX`（更老的版本用 `ClipboardManager`）；
//!   管理员模式下改为计划任务 `ClipboardX_AutoStart`（`StartupRegistration.cs`）
//! - 互斥体：`ClipboardX_F7A2E9B0`（`AppPaths.MutexName`，FULL flavor）
//! - 安装目录：`%LocalAppData%\Programs\ClipboardX`；数据根 `%LocalAppData%\ClipboardX`

use std::path::PathBuf;

/// 老版 FULL flavor 的单实例互斥体名（`AppPaths.MutexName`，CLIPX_FULL 分支）。
const LEGACY_MUTEX_FULL: &str = "ClipboardX_F7A2E9B0";
/// 老版「仅剪贴板」flavor 的互斥体名。
const LEGACY_MUTEX_CLIPBOARD: &str = "ClipboardX_Clipboard_A1B2C3D4";
/// 老版「文件跳转」flavor 的互斥体名。
const LEGACY_MUTEX_FILEJUMP: &str = "ClipboardX_FileJump_E5F6G7H8";

/// HKCU Run 子键路径。
const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 老版自启值名（现行）与更早版本的值名。
const RUN_VALUE_NAMES: [&str; 2] = ["ClipboardX", "ClipboardManager"];
/// 老版管理员模式下的登录自启计划任务名。
///
/// 除正式版外还有 Dev 变体（老版 `StartupRegistration.cs` 按构建 flavor 拼后缀，
/// 本机实测存在 `ClipboardX_AutoStart_Dev`）——两个都要认，否则 dev 用户漏网。
const SCHEDULED_TASKS: [&str; 2] = ["ClipboardX_AutoStart", "ClipboardX_AutoStart_Dev"];

/// 老版存在性/运行态快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyStatus {
    /// 按用户安装目录是否存在（`%LocalAppData%\Programs\ClipboardX`）。
    pub installed: bool,
    /// 安装目录里的主程序 exe 路径（存在时）。
    pub exe: Option<PathBuf>,
    /// 数据目录 `%LocalAppData%\ClipboardX` 是否存在（历史库所在）。
    pub data_dir: bool,
    /// 老版进程是否在跑（按互斥体判定，覆盖三个 flavor）。
    pub running: bool,
    /// 命中的 HKCU Run 值名（存在自启时才非空）。
    pub run_values: Vec<String>,
    /// 登录自启计划任务是否存在。
    pub scheduled_task: bool,
}

impl LegacyStatus {
    /// 用户机器上是否有老版痕迹（安装目录 / 数据 / 自启 / 在跑，任一命中）。
    pub fn present(&self) -> bool {
        self.installed || self.data_dir || self.running || !self.run_values.is_empty()
    }

    /// 是否还有需要停用的自启项。
    pub fn has_autostart(&self) -> bool {
        !self.run_values.is_empty() || self.scheduled_task
    }
}

/// 老版安装目录（按用户安装）。
pub fn install_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("Programs").join("ClipboardX"))
}

/// 老版数据目录（历史库与 settings.json 所在）。**只读探测，绝不删除。**
pub fn data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("ClipboardX"))
}

/// 老版主程序 exe 候选名（FULL 与两个精简 flavor）。
const LEGACY_EXE_NAMES: [&str; 3] = [
    "ClipboardX.exe",
    "ClipboardX-clipboard.exe",
    "ClipboardX-filejump.exe",
];

/// 迁移收尾的日志：**不受 `CLIPX_DEBUG` 门控**。
///
/// 迁移是一次性、低频、且发生在用户第一次打开新版本时——那时候没人会去设
/// 调试环境变量，事后也几乎无法复现。所以检测与停用的每一步都必须无条件留痕，
/// 否则出问题只能靠猜。`wpf_import.log` 里现在混有两类记录：导入（debug 门控，
/// 沿用原样）与迁移收尾（本函数，恒写）。
pub fn log(line: &str) {
    crate::win_popup::write_data_log("wpf_import.log", line);
}

/// 探测老版现状。全部失败都视为「不存在」，不 panic、不阻断启动。
pub fn detect() -> LegacyStatus {
    let mut st = LegacyStatus::default();

    if let Some(dir) = install_dir() {
        for name in LEGACY_EXE_NAMES {
            let exe = dir.join(name);
            if exe.is_file() {
                st.installed = true;
                st.exe = Some(exe);
                break;
            }
        }
    }
    if let Some(d) = data_dir() {
        st.data_dir = d.join("clipboard_history.db").is_file();
    }

    #[cfg(windows)]
    {
        st.run_values = read_run_values();
        st.scheduled_task = !find_scheduled_tasks().is_empty();
        st.running = legacy_running();
    }

    st
}

/// 读 HKCU Run 键里老版的值名（返回命中的名字）。
#[cfg(windows)]
fn read_run_values() -> Vec<String> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
        REG_SZ,
    };

    let mut out = Vec::new();
    unsafe {
        let mut hkey = HKEY::default();
        let sub = to_wide(RUN_SUBKEY);
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut hkey,
        )
        .is_err()
        {
            return out;
        }
        for name in RUN_VALUE_NAMES {
            let wname = to_wide(name);
            let mut ty = REG_SZ;
            // 先探大小（数据传 None），拿到存在性即足够——我们只要值名，不要内容。
            let rc = RegQueryValueExW(
                hkey,
                windows::core::PCWSTR(wname.as_ptr()),
                None,
                Some(&mut ty),
                None,
                None,
            );
            if rc.is_ok() {
                out.push(name.to_string());
            }
        }
        let _ = RegCloseKey(hkey);
    }
    out
}

#[cfg(not(windows))]
fn read_run_values() -> Vec<String> {
    Vec::new()
}

/// 老版是否在跑：尝试打开它的单实例互斥体（三个 flavor 任一）。
#[cfg(windows)]
fn legacy_running() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, MUTEX_ALL_ACCESS};

    unsafe {
        for name in [
            LEGACY_MUTEX_FULL,
            LEGACY_MUTEX_CLIPBOARD,
            LEGACY_MUTEX_FILEJUMP,
        ] {
            let w = to_wide(name);
            // 老版用 CreateMutexW 创建具名互斥体；我们 OpenMutexW 能打开 = 它活着。
            if let Ok(h) = OpenMutexW(MUTEX_ALL_ACCESS, false, PCWSTR(w.as_ptr())) {
                let _ = CloseHandle(h);
                return true;
            }
            // 兜底：CreateMutexW 已存在时返回 handle + LastError=ERROR_ALREADY_EXISTS，
            // 有些老版本建在会话命名空间之外，Open 打不开时这条路仍能判出来。
            if let Ok(h) = CreateMutexW(None, false, PCWSTR(w.as_ptr())) {
                let existed = windows::core::Error::from_win32().code() == ERROR_ALREADY_EXISTS.to_hresult();
                let _ = CloseHandle(h);
                if existed {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(not(windows))]
fn legacy_running() -> bool {
    false
}

/// 老版登录自启计划任务是否存在（`schtasks /Query`），返回命中的任务名。
///
/// 逐个查（`schtasks` 一次只认一个 `/TN`），命中即收；正式版与 Dev 版都要认。
#[cfg(windows)]
fn find_scheduled_tasks() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut hit = Vec::new();
    for name in SCHEDULED_TASKS {
        let exists = std::process::Command::new("schtasks")
            .args(["/Query", "/TN", name])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if exists {
            hit.push(name.to_string());
        }
    }
    hit
}

#[cfg(not(windows))]
fn find_scheduled_tasks() -> Vec<String> {
    Vec::new()
}

/// 停用结果：报告每一项的实际结果，让调用方如实提示（别只说「已停用」）。
#[derive(Debug, Clone, Default)]
pub struct DisableOutcome {
    /// 已删除的 HKCU Run 值名。
    pub run_removed: Vec<String>,
    /// 已成功删除的计划任务名。
    pub tasks_removed: Vec<String>,
    /// 删除失败的计划任务名（多半是权限不够）。
    pub tasks_failed: Vec<String>,
    /// 失败项的说明（给用户看）。
    pub errors: Vec<String>,
}

impl DisableOutcome {
    pub fn changed(&self) -> bool {
        !self.run_removed.is_empty() || !self.tasks_removed.is_empty()
    }
}

/// 停用老版开机自启：删 HKCU Run 值 + 删登录计划任务。
///
/// **绝不删除程序目录与数据目录**——历史库要留着让用户确认迁移结果后再卸载。
pub fn disable_autostart() -> DisableOutcome {
    let mut out = DisableOutcome::default();

    #[cfg(windows)]
    {
        out.run_removed = remove_run_values();
        for name in find_scheduled_tasks() {
            match delete_scheduled_task(&name) {
                Ok(()) => out.tasks_removed.push(name),
                Err(e) => {
                    out.tasks_failed.push(name.clone());
                    out.errors.push(format!("删除计划任务 {name} 失败：{e}"));
                }
            }
        }
    }

    out
}

/// 删除 HKCU Run 里的老版值（返回成功删掉的值名）。
#[cfg(windows)]
fn remove_run_values() -> Vec<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };

    let mut removed = Vec::new();
    unsafe {
        let mut hkey = HKEY::default();
        let sub = to_wide(RUN_SUBKEY);
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut hkey,
        )
        .is_err()
        {
            return removed;
        }
        for name in RUN_VALUE_NAMES {
            let w = to_wide(name);
            if RegDeleteValueW(hkey, PCWSTR(w.as_ptr())).is_ok() {
                removed.push(name.to_string());
            }
        }
        let _ = RegCloseKey(hkey);
    }
    removed
}

/// 删除登录自启计划任务（按名）。
///
/// 注意：老版的管理员模式自启任务是以提升权限注册的，**普通权限删不掉**
/// （`schtasks` 返回「拒绝访问」）。此时如实告知「需管理员权限」，
/// 不把原始 OEM 乱码抛给用户看。
#[cfg(windows)]
fn delete_scheduled_task(name: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("schtasks")
        .args(["/Delete", "/F", "/TN", name])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let raw = decode_oem(&out.stderr);
        let raw = raw.trim();
        // 拒绝访问（中英文两版 Windows 都要认）→ 给可操作的提示。
        if raw.contains("拒绝访问") || raw.to_ascii_lowercase().contains("access is denied") {
            Err("需管理员权限".to_string())
        } else if raw.is_empty() {
            Err("schtasks 返回失败但无错误信息".to_string())
        } else {
            Err(raw.to_string())
        }
    }
}

/// 把控制台程序（schtasks 等）的输出按系统 OEM 代码页解码成 UTF-8 字符串。
///
/// 这些程序输出的是 ANSI/OEM 字节（简体中文 Windows 上是 GBK），
/// 直接 `from_utf8_lossy` 会得到一串 `?`——错误信息就废了。
#[cfg(windows)]
fn decode_oem(bytes: &[u8]) -> String {
    use windows::Win32::Globalization::{MultiByteToWideChar, CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
    if bytes.is_empty() {
        return String::new();
    }
    unsafe {
        let need = MultiByteToWideChar(
            CP_OEMCP,
            MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
            bytes,
            None,
        );
        if need <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut buf = vec![0u16; need as usize];
        let got = MultiByteToWideChar(
            CP_OEMCP,
            MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
            bytes,
            Some(&mut buf),
        );
        if got <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

/// UTF-16 零结尾宽字符（Windows API 用）。
#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_flag_covers_all_signals() {
        let empty = LegacyStatus::default();
        assert!(!empty.present());
        assert!(!empty.has_autostart());

        let installed = LegacyStatus {
            installed: true,
            ..Default::default()
        };
        assert!(installed.present());
        assert!(!installed.has_autostart(), "装了但没自启不算待停用");

        let with_run = LegacyStatus {
            run_values: vec!["ClipboardX".into()],
            ..Default::default()
        };
        assert!(with_run.present());
        assert!(with_run.has_autostart());

        let task_only = LegacyStatus {
            scheduled_task: true,
            ..Default::default()
        };
        assert!(task_only.has_autostart());
    }

    #[test]
    fn outcome_changed_only_on_real_effect() {
        let none = DisableOutcome::default();
        assert!(!none.changed());

        let run = DisableOutcome {
            run_removed: vec!["ClipboardX".into()],
            ..Default::default()
        };
        assert!(run.changed());

        // 任务本来就没找到（空列表）+ 没删 Run 值 = 没变化
        let noop = DisableOutcome {
            tasks_removed: Vec::new(),
            ..Default::default()
        };
        assert!(!noop.changed());

        let failed = DisableOutcome {
            tasks_failed: vec!["ClipboardX_AutoStart".into()],
            errors: vec!["boom".into()],
            ..Default::default()
        };
        assert!(!failed.changed(), "失败不算变化");

        let task_ok = DisableOutcome {
            tasks_removed: vec!["ClipboardX_AutoStart".into()],
            ..Default::default()
        };
        assert!(task_ok.changed());
    }

    #[test]
    fn scheduled_task_names_cover_dev_variant() {
        // 老版 dev 构建的自启任务是 `ClipboardX_AutoStart_Dev`（本机实测存在），
        // 只清正式版会漏掉 dev 用户的自启。
        assert!(SCHEDULED_TASKS.contains(&"ClipboardX_AutoStart"));
        assert!(SCHEDULED_TASKS.contains(&"ClipboardX_AutoStart_Dev"));
    }

    #[test]
    fn paths_point_at_legacy_locations() {
        if let Some(dir) = install_dir() {
            assert!(dir.ends_with("Programs/ClipboardX") || dir.ends_with(r"Programs\ClipboardX"));
        }
        if let Some(dir) = data_dir() {
            assert!(dir.ends_with("ClipboardX"));
        }
    }

    /// 真机探针：把本机老版现状打出来（不是断言，是给人看的）。
    /// `cargo test -p clipx-app legacy_wpf_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn legacy_wpf_probe() {
        let st = detect();
        println!("installed     = {}", st.installed);
        println!("exe           = {:?}", st.exe);
        println!("data_dir      = {}", st.data_dir);
        println!("running       = {}", st.running);
        println!("run_values    = {:?}", st.run_values);
        println!("sched_task    = {}", st.scheduled_task);
        println!("present       = {}", st.present());
        println!("has_autostart = {}", st.has_autostart());
    }
}
