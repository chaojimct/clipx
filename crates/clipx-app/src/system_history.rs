//! 系统剪贴板历史回补（方案 B，Windows Only）。
//!
//! 背景：采集与 UI 同进程，退出期间的复制无人监听。uTools 那类“关了也有”
//! 其实是常驻后台偷跑；我们不做常驻，退而求其次——启动时把 Win+V 系统历史
//! 里有、而我们库里没有的文本条目补回来（图片/文件不在系统历史里，补不了）。
//!
//! 前提（缺一则静默跳过，不打扰启动）：
//! - `HKCU\Software\Microsoft\Clipboard\EnableClipboardHistory = 1`
//!   （旧版本开“替换 Win+V”会写 0，见 `heal_history_switch_once` 一次性治愈；
//!   新版本只拦截热键不再动这个开关，两边共存）
//! - `GetHistoryItemsAsync` 返回 `Success`（无焦点也可用已在真机验证；
//!   `AccessDenied` / `ClipboardHistoryDisabled` 直接跳过）
//! - 用户没开 `clear_history_on_exit`（开了=明确要空库回补会复活历史必须让路）。
//!
//! 入库走 `Store::import_batch`（与 WPF 迁移同一条路）：按 `content_hash` 去重
//! 跳过、保留系统时间戳、不 bump 现有条目顺序。

use std::path::Path;
use std::sync::mpsc::Sender;

use crate::logic::AppEvt;

/// 系统历史最多取回的条数（系统侧本来也就只留 ~25 条）。
const MAX_BACKFILL: usize = 50;
/// 单条文本上限（剪贴板里偶有整篇文档按 MB 计，库里不收这种巨物）。
const MAX_TEXT_BYTES: usize = 1_000_000;
/// 启动后延迟回补（让监听先把当前剪贴板吃进去、让 WPF 导入先跑别挤启动）。
const BACKFILL_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
/// 一次性治愈标记：旧版本写 0 的开关只修一次之后用户手动关的不再碰。
const HEAL_MARKER: &str = ".syshist_healed";

/// 旧版本“替换 Win+V”会把系统历史开关写 0（且退出写回 1 时记录已被清空）。
/// 新版本不再写 0，但已中招的机器还停在 0：一次性翻回 1，之后不再干预。
/// 只修 0→1，从不动 1→0；用户之后手动关的与我们无关。
#[cfg(windows)]
pub fn heal_history_switch_once(data_dir: &Path) {
    if data_dir.join(HEAL_MARKER).is_file() {
        return;
    }
    let healed = match read_history_switch() {
        Some(true) => true, // 本来就是开的，只落标记
        Some(false) => {
            let ok = write_history_switch(true);
            crate::win_popup::append_debug_log(
                "syshistory.log",
                &format!("heal EnableClipboardHistory 0->1 ok={ok}（旧版本替换 Win+V 写 0 的遗留）"),
            );
            ok
        }
        None => false, // 读不到不乱动下次启动再试
    };
    if healed {
        let _ = std::fs::write(data_dir.join(HEAL_MARKER), "1");
    }
}

#[cfg(not(windows))]
pub fn heal_history_switch_once(_data_dir: &Path) {}

/// 后台线程：延迟 → 读系统历史 → `import_batch` → 有新增才回投提示。
/// 任何失败都只记 `syshistory.log`不打断启动、不弹错。
pub fn spawn_backfill(store: clipx_store::Store, evt_tx: Sender<AppEvt>) {
    let spawn = std::thread::Builder::new()
        .name("clipx-syshist".into())
        .spawn(move || {
            std::thread::sleep(BACKFILL_DELAY);
            match backfill_once(&store) {
                Ok((inserted, skipped)) => {
                    crate::win_popup::append_debug_log(
                        "syshistory.log",
                        &format!("backfill done inserted={inserted} dup={skipped}"),
                    );
                    if inserted > 0 {
                        let _ = evt_tx.send(AppEvt::SysHistoryBackfilled {
                            inserted,
                            skipped_dup: skipped,
                        });
                    }
                }
                Err(e) => {
                    crate::win_popup::append_debug_log(
                        "syshistory.log",
                        &format!("backfill skip: {e}"),
                    );
                }
            }
        });
    if let Err(e) = spawn {
        eprintln!("启动系统历史回补线程失败: {e}");
    }
}

/// 读系统历史 → 组 `MigrationRow` → `import_batch`。返回（新增， 去重跳过）。
#[cfg(windows)]
fn backfill_once(store: &clipx_store::Store) -> anyhow::Result<(usize, usize)> {
    use anyhow::Context;

    let items = read_os_history().context("读系统剪贴板历史失败")?;
    if items.is_empty() {
        return Ok((0, 0));
    }
    let mut rows = Vec::new();
    for (text, created_ms) in items.into_iter().take(MAX_BACKFILL) {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            continue;
        }
        let mut entry = clipx_core::NewEntry::from_text(text);
        entry.source_app = "系统历史".to_string();
        rows.push(clipx_store::MigrationRow {
            entry,
            created_ms,
            ocr_text: None,
        });
    }
    if rows.is_empty() {
        return Ok((0, 0));
    }
    let stats = store
        .import_batch(rows)
        .context("系统历史入库失败")?;
    Ok((stats.inserted, stats.skipped_dup))
}

#[cfg(not(windows))]
fn backfill_once(_store: &clipx_store::Store) -> anyhow::Result<(usize, usize)> {
    anyhow::bail!("非 Windows 无系统剪贴板历史")
}

/// WinRT `GetHistoryItemsAsync` 同步读（后台 MTA 线程阻塞等）。
/// 只取文本；Bitmap 等非文本格式跳过（转 PNG 代价大且库里已有实时采集）。
/// 返回（正文， 系统时间戳毫秒）新→旧不保证，入库按 `created_ms` 排序与顺序无关。
#[cfg(windows)]
fn read_os_history() -> anyhow::Result<Vec<(String, i64)>> {
    use anyhow::Context;
    use windows::ApplicationModel::DataTransfer as dt;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_SINGLETHREADED};

    // Clipboard WinRT 类是 STA 亲和（MTA 直接调报 0x8000001D，已实测）：
    // 回补线程只干这一件事，阻塞等完成即可（`get()` 是事件等待，
    // 完成回调走 RPC 线程不依赖本线程抽消息，真机验证不死锁）。
    unsafe {
        // 已初始化（比如单测线程复用）则返回 S_FALSE/RPC_E_CHANGED_MODE，忽略即可。
        let _ = RoInitialize(RO_INIT_SINGLETHREADED);
    }
    if !dt::Clipboard::IsHistoryEnabled().context("IsHistoryEnabled 调用失败")? {
        anyhow::bail!("系统剪贴板历史未开启");
    }
    let res = dt::Clipboard::GetHistoryItemsAsync()
        .context("GetHistoryItemsAsync 发起失败")?
        .get()
        .context("GetHistoryItemsAsync 等待失败")?;
    match res.Status().context("Status 读取失败")? {
        dt::ClipboardHistoryItemsResultStatus::Success => {}
        dt::ClipboardHistoryItemsResultStatus::AccessDenied => {
            anyhow::bail!("系统历史访问被拒绝（AccessDenied）")
        }
        dt::ClipboardHistoryItemsResultStatus::ClipboardHistoryDisabled => {
            anyhow::bail!("系统剪贴板历史已禁用")
        }
        s => anyhow::bail!("系统历史返回未知状态: {s:?}"),
    }
    let text_fmt = dt::StandardDataFormats::Text().context("Text 格式名获取失败")?;
    let items = res.Items().context("Items 读取失败")?;
    let n = items.Size().context("Size 读取失败")?;
    let mut out = Vec::new();
    for i in 0..n {
        let it = match items.GetAt(i) {
            Ok(it) => it,
            Err(_) => continue,
        };
        let content = match it.Content() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let has_text = content.Contains(&text_fmt).unwrap_or(false);
        if !has_text {
            continue; // 图片等非文本跳过
        }
        let text = match content.GetTextAsync().and_then(|op| op.get()) {
            Ok(t) => t.to_string(),
            Err(_) => continue,
        };
        let created_ms = it
            .Timestamp()
            .map(filetime_to_unix_ms)
            .unwrap_or_else(|_| chrono_now_ms());
        out.push((text, created_ms));
    }
    Ok(out)
}

/// WinRT `DateTime.UniversalTime`（1601 起 100ns  tick）→ Unix 毫秒。
#[cfg(windows)]
fn filetime_to_unix_ms(ut: windows::Foundation::DateTime) -> i64 {
    const TICKS_PER_MS: i64 = 10_000;
    const EPOCH_DIFF_MS: i64 = 11_644_473_600_000; // 1601-01-01 → 1970-01-01
    ut.UniversalTime / TICKS_PER_MS - EPOCH_DIFF_MS
}

#[cfg(windows)]
fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 读 `EnableClipboardHistory`（缺键/读失败返回 `None` 不乱猜）。
#[cfg(windows)]
fn read_history_switch() -> Option<bool> {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
    };

    let mut val: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"SOFTWARE\Microsoft\Clipboard"),
            w!("EnableClipboardHistory"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut val as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if err == ERROR_SUCCESS {
        Some(val == 1)
    } else {
        None
    }
}

/// 写 `EnableClipboardHistory`（只用于 0→1 的治愈从不写 0）。
#[cfg(windows)]
fn write_history_switch(enabled: bool) -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_CREATE_KEY_DISPOSITION, REG_DWORD, REG_OPTION_NON_VOLATILE,
    };

    debug_assert!(enabled, "此函数只允许 0→1 不允许关闭系统历史");
    unsafe {
        let mut hkey = HKEY::default();
        let mut disp = REG_CREATE_KEY_DISPOSITION(0);
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Clipboard"),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            Some(&mut disp),
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let val: u32 = 1;
        let ok = RegSetValueExW(
            hkey,
            w!("EnableClipboardHistory"),
            Some(0),
            REG_DWORD,
            Some(&val.to_le_bytes()),
        ) == ERROR_SUCCESS;
        let _ = RegCloseKey(hkey);
        ok
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(windows)]
    fn filetime_epoch_math() {
        use super::filetime_to_unix_ms;
        // 1970-01-01T00:00:00Z 对应 tick = 11644473600000ms * 10000
        let ut = windows::Foundation::DateTime {
            UniversalTime: 11_644_473_600_000 * 10_000,
        };
        assert_eq!(filetime_to_unix_ms(ut), 0);
        // 2026-10-27T00:00:00Z = 1793030400000ms（python: datetime(2026,10,27).timestamp()*1000）
        let ut = windows::Foundation::DateTime {
            UniversalTime: (1793030400000 + 11_644_473_600_000) * 10_000,
        };
        assert_eq!(filetime_to_unix_ms(ut), 1793030400000);
    }

    /// 真机探针：打印系统历史条数与首条（需开系统历史）。
    /// `cargo test -p clipx-app syshist_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn syshist_probe() {
        use super::{read_history_switch, read_os_history};
        match read_os_history() {
            Ok(items) => {
                println!("os-history count = {}", items.len());
                for (t, ms) in items.iter().take(3) {
                    let head: String = t.chars().take(40).collect();
                    println!("  ms={ms} text={head:?}");
                }
            }
            Err(e) => println!("os-history read failed: {e:#}"),
        }
        println!("switch = {:?}", read_history_switch());
    }
}
