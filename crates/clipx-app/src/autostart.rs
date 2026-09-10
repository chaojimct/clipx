//! 开机自启：schtasks XML 导入方案（对齐 WPF v1.9.8 的注册方式）。
//!
//! 用 XML 而非命令行参数注册，规避 v1.9.7 的三个坑：
//! 1. 命令行 /TR 带引号路径 → 登录时弹 cmd 黑窗；
//! 2. 默认 72 小时执行时限 → 常驻进程被强杀；
//! 3. 默认电池模式禁启动、任何用户登录都触发。
//!
//! XML 方案：不限定执行时长、电池可用、仅当前用户登录触发、路径不加引号。

/// 计划任务名（与 WPF 版任务互不冲突）
#[cfg(windows)]
const TASK_NAME: &str = "clipx-autostart";

/// 控制台子进程不闪黑窗（schtasks/whoami 每次保存设置都会调一次）。
#[cfg(windows)]
fn silent(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW
    cmd.creation_flags(0x0800_0000);
}

#[cfg(windows)]
pub fn is_enabled() -> bool {
    let mut cmd = std::process::Command::new("schtasks");
    silent(&mut cmd);
    let out = cmd
        .args(["/Query", "/TN", TASK_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    matches!(out, Ok(st) if st.success())
}

/// 翻转自启状态；返回翻转后的新状态，None = schtasks 调用失败。
#[cfg(windows)]
pub fn toggle() -> Option<bool> {
    if is_enabled() {
        disable().then_some(false)
    } else {
        enable(false).then_some(true)
    }
}

/// 按设置应用自启（WPF `StartupRegistration.Apply` 简化版）：
/// 关 → 删任务；开 → 按 admin 决定是否 HighestAvailable。
#[cfg(windows)]
pub fn set(on: bool, admin: bool) -> bool {
    if !on {
        return disable();
    }
    if is_enabled() {
        // 已注册：重建以同步 admin 标志。
        disable();
    }
    enable(admin)
}

#[cfg(windows)]
fn enable(admin: bool) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(path) = exe.to_str() else {
        return false;
    };
    let user = whoami_local().unwrap_or_default();
    if user.is_empty() {
        return false;
    }

    // 管理员模式加 HighestAvailable（WPF `RunAsAdministrator` 语义）。
    let principal = if admin {
        r#"  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
"#
        .replace("{user}", &user)
    } else {
        String::new()
    };
    // 路径不含引号（含空格也合法：Command 元素是整个执行串）
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
{principal}  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <StartWhenAvailable>false</StartWhenAvailable>
  </Settings>
  <Actions>
    <Exec>
      <Command>{path}</Command>
    </Exec>
  </Actions>
</Task>
"#
    );

    // schtasks /Create /XML 要求 UTF-16 LE（含 BOM）
    let utf16: Vec<u8> = xml.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    let tmp = std::env::temp_dir().join("clipx-autostart.xml");
    if std::fs::write(&tmp, [vec![0xFF, 0xFE], utf16].concat()).is_err() {
        return false;
    }

    let mut cmd = std::process::Command::new("schtasks");
    silent(&mut cmd);
    let ok = cmd
        .args([
            "/Create",
            "/TN",
            TASK_NAME,
            "/XML",
            &tmp.to_string_lossy(),
            "/F",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp);
    ok
}

#[cfg(windows)]
fn disable() -> bool {
    let mut cmd = std::process::Command::new("schtasks");
    silent(&mut cmd);
    cmd.args(["/Delete", "/TN", TASK_NAME, "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 当前用户 DOMAIN\name（LogonTrigger 限定触发者，避免其他用户登录也拉起）
#[cfg(windows)]
fn whoami_local() -> Option<String> {
    let mut cmd = std::process::Command::new("whoami");
    silent(&mut cmd);
    let out = cmd.output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn toggle() -> Option<bool> {
    None
}

/// 非 Windows：M6 用 SMAppService 实现（CROSSPLATFORM.md §1.6），当前固定未启用。
#[cfg(not(windows))]
pub fn set(_on: bool, _admin: bool) -> bool {
    false
}

#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = Default::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut n = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elev as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// 发布版才按「以管理员运行」自动 UAC 提权。
/// debug 每次 `cargo run` 弹 UAC 没必要：低级钩子/剪贴板采集不依赖提升，
/// 提权后 cargo 子进程立刻退出，管理员实例还会锁住 exe 导致编不过。
pub fn should_auto_elevate() -> bool {
    !cfg!(debug_assertions)
}

/// 以管理员身份再启一份（UAC）。成功则调用方应退出。
#[cfg(windows)]
pub fn restart_elevated() -> bool {
    restart_with_verb(true)
}

/// 从已提升进程拉起非提升实例。成功则调用方应退出。
#[cfg(windows)]
pub fn restart_unelevated() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("dotnet.exe"))
    {
        return false;
    }
    let path = exe.to_string_lossy();
    let mut cmd = std::process::Command::new("cmd");
    silent(&mut cmd);
    cmd.args(["/c", "start", "", &path, "--restart"]);
    cmd.spawn().is_ok()
}

#[cfg(windows)]
fn restart_with_verb(elevated: bool) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::{ShellExecuteW, SEE_MASK_NOCLOSEPROCESS};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("dotnet.exe"))
    {
        return false;
    }
    let file = HSTRING::from(exe.as_os_str());
    let args = HSTRING::from("--restart");
    let verb = if elevated {
        HSTRING::from("runas")
    } else {
        HSTRING::from("open")
    };
    let _ = SEE_MASK_NOCLOSEPROCESS;
    unsafe {
        let ret = ShellExecuteW(None, &verb, &file, &args, None, SW_SHOWNORMAL);
        ret.0 as isize > 32
    }
}

#[cfg(not(windows))]
pub fn restart_elevated() -> bool {
    false
}

#[cfg(not(windows))]
pub fn restart_unelevated() -> bool {
    false
}
