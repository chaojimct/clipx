//! 跨平台「快速跳转」执行层：在文件管理器中定位（reveal）与打开（open）。
//!
//! QuickFind/搜索结果跳转的跨平台执行端：
//! - Windows：`SHParseDisplayName` + `SHOpenFolderAndSelectItems` / `ShellExecuteW`
//!   （`clipx-filejump`/`explorer_shell` 的深跳转路径不变，本 crate 供跨平台统一入口）
//! - macOS：`open -R`（Finder 定位）/ `open`
//! - Linux X11：`nautilus --select` / `dolphin --select` / 回退 `xdg-open`（打开父目录）
//! - Wayland：命令层同样可用（`xdg-open`），仅键盘注入类能力缺失
//!
//! 对话框内跳转（打开/保存框）按 CROSSPLATFORM.md §1.4 后续在此扩展。

use anyhow::{bail, Result};

/// 在系统的文件管理器中显示该路径（文件 → 父目录中选中；目录 → 选中该目录）。
/// 返回 false 表示平台无可用文件管理器（如最小化 Linux 环境）。
pub fn reveal(path: &str) -> Result<bool> {
    if path.trim().is_empty() {
        bail!("空路径");
    }
    if !std::path::Path::new(path).exists() {
        bail!("路径不存在: {path}");
    }
    reveal_impl(path)
}

/// 用系统默认方式打开路径（文件按关联程序，目录进文件管理器）。
pub fn open_path(path: &str) -> Result<bool> {
    if path.trim().is_empty() {
        bail!("空路径");
    }
    if !std::path::Path::new(path).exists() {
        bail!("路径不存在: {path}");
    }
    open_impl(path)
}

// ===== Windows：Shell API（线程内自初始化 COM，已初始化则忽略） =====

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        ILFree, SHOpenFolderAndSelectItems, ShellExecuteW, SHParseDisplayName,
    };
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn reveal_impl(path: &str) -> Result<bool> {
        unsafe {
            let co = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let wide = to_wide(path);
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            let r = SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None);
            if r.is_err() || pidl.is_null() {
                if co.is_ok() {
                    CoUninitialize();
                }
                bail!("SHParseDisplayName 失败: {path}");
            }
            let opened = SHOpenFolderAndSelectItems(pidl, None, 0);
            ILFree(Some(pidl));
            if co.is_ok() {
                CoUninitialize();
            }
            Ok(opened.is_ok())
        }
    }

    pub(super) fn open_impl(path: &str) -> Result<bool> {
        unsafe {
            let wide = to_wide(path);
            let r = ShellExecuteW(
                Some(HWND::default()),
                w!("open"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
            // SE_ERR 值 ≤32 表示失败
            Ok(r.0 as isize > 32)
        }
    }
}

// ===== macOS / Linux：系统命令（候选依次尝试，命令不存在静默跳过） =====

#[cfg(unix)]
mod platform {
    use super::*;

    fn run_ok(program: &str, args: &[&str]) -> bool {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// reveal 候选命令（由高到低）。独立成纯函数便于测试。
    fn reveal_candidates(path: &str) -> Vec<(String, Vec<String>)> {
        #[cfg(target_os = "macos")]
        {
            vec![("open".into(), vec!["-R".into(), path.into()])]
        }
        #[cfg(not(target_os = "macos"))]
        {
            let parent = std::path::Path::new(path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".into());
            vec![
                ("nautilus".into(), vec!["--select".into(), path.into()]),
                ("dolphin".into(), vec!["--select".into(), path.into()]),
                ("xdg-open".into(), vec![parent]),
            ]
        }
    }

    pub(super) fn reveal_impl(path: &str) -> Result<bool> {
        for (program, args) in reveal_candidates(path) {
            let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            if run_ok(&program, &refs) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn open_impl(path: &str) -> Result<bool> {
        #[cfg(target_os = "macos")]
        {
            Ok(run_ok("open", &[path]))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(run_ok("xdg-open", &[path]))
        }
    }
}

#[cfg(windows)]
use platform::{open_impl, reveal_impl};
#[cfg(unix)]
use platform::{open_impl, reveal_impl};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_missing_paths() {
        assert!(reveal("").is_err());
        assert!(open_path("").is_err());
        assert!(reveal("./surely-not-exist-某路径").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn reveal_candidates_have_fallback_chain() {
        let c = platform::reveal_candidates("/tmp/x.txt");
        assert!(c.len() >= 2, "至少含选中与打开目录两级回退");
        assert_eq!(c[0].0, "nautilus");
        assert_eq!(c[0].1, vec!["--select", "/tmp/x.txt"]);
        let last = c.last().expect("fallback");
        assert_eq!(last.0, "xdg-open");
    }

    /// 真实打开资源管理器定位临时文件（交互环境手动跑：cargo test -p clipx-jump -- --ignored）
    #[test]
    #[ignore]
    fn reveal_smoke() {
        let dir = std::env::temp_dir().join("clipx-jump-smoke");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("定位测试.txt");
        std::fs::write(&f, b"x").ok();
        let r = reveal(f.to_string_lossy().as_ref()).expect("reveal");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r);
    }
}
