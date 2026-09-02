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

#[cfg(windows)]
pub fn is_enabled() -> bool {
    let out = std::process::Command::new("schtasks")
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
        enable().then_some(true)
    }
}

#[cfg(windows)]
fn enable() -> bool {
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

    // 路径不含引号（含空格也合法：Command 元素是整个执行串）
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
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

    let ok = std::process::Command::new("schtasks")
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
    std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 当前用户 DOMAIN\name（LogonTrigger 限定触发者，避免其他用户登录也拉起）
#[cfg(windows)]
fn whoami_local() -> Option<String> {
    let out = std::process::Command::new("whoami").output().ok()?;
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
