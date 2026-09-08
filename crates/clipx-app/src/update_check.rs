//! 启动约 45s 后静默查 GitHub Releases（对齐 WPF）。
//! 有新版且 tag 与 `last_update_tag` 不同时托盘提示一次。

use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::logic::AppEvt;

const API: &str = "https://api.github.com/repos/chaojimct/clipboardx/releases/latest";

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
            if let Some(tag) = fetch_latest_tag() {
                if last_tag.as_deref() != Some(tag.as_str()) && tag != env!("CARGO_PKG_VERSION") {
                    let _ = tx.send(AppEvt::UpdateAvailable(tag));
                }
            }
        });
}

fn fetch_latest_tag() -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "try {{ (Invoke-RestMethod -Uri '{API}' -Headers @{{'User-Agent'='clipx'}}).tag_name }} catch {{ }}"
                ),
            ])
            // CREATE_NO_WINDOW：45s 静默检查不能闪黑窗
            .creation_flags(0x0800_0000)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if tag.is_empty() {
            None
        } else {
            Some(tag.trim_start_matches('v').to_string())
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}
