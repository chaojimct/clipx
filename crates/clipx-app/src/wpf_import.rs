//! WPF 版历史的首启自动导入。
//!
//! 目标（PRD「数据迁移」）：装完 clipx 打开就能看到老版的历史，不需要命令行。
//! 触发条件：候选源库存在，且本数据目录没写过导入标记 `.wpf-import.json`。
//!
//! 重复运行是安全的——`import_batch` 按 `content_hash` 去重，已存在的行直接跳过；
//! 标记只是省掉白跑一次的开销，不是正确性依赖。手动重跑入口仍是
//! `clipx --import-wpf <clipboard_history.db>`。
//!
//! 内存：源库实测 197MB / 7002 条（图片 blob 47MB），故走 [`BatchReader`] 分批，
//! 每批受「行数 + blob 字节」双重限制，峰值不会随源库增长。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use clipx_store::Store;

use crate::logic::AppEvt;
use crate::settings::Settings;

/// 每批行数上限。
const BATCH_ROWS: usize = 200;
/// 每批图片 blob 字节上限（控制峰值内存）。
const BATCH_BYTES: usize = 24 * 1024 * 1024;
/// 批间让位：store 是单线程 channel，导入期间 UI 查询会排队，留空隙给它们插队。
const BATCH_GAP: std::time::Duration = std::time::Duration::from_millis(20);

#[cfg(windows)]
fn local_appdata() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

#[cfg(not(windows))]
fn local_appdata() -> Option<PathBuf> {
    // WPF 版是 Windows 专属；mac/Linux 上只有用户手动拷来的便携库可能命中。
    None
}

/// 候选源库路径（按优先级）。两个位置都可能：
/// - WPF 安装模式：`%LocalAppData%\ClipboardX\clipboard_history.db`（不带 Data 子目录）
/// - WPF 便携模式：`<WPF exe>\Data\clipboard_history.db`
pub fn candidates(app_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(local) = local_appdata() {
        push(local.join("ClipboardX").join("clipboard_history.db"));
        push(
            local
                .join("ClipboardX")
                .join("Data")
                .join("clipboard_history.db"),
        );
    }
    // 便携对便携（clipx 与 WPF 解压在同一层）；开发机则是 ../clipboard/Data。
    push(app_dir.join("Data").join("clipboard_history.db"));
    if let Some(parent) = app_dir.parent() {
        push(parent.join("clipboard").join("Data").join("clipboard_history.db"));
        push(parent.join("Data").join("clipboard_history.db"));
    }
    out
}

/// 找到第一个存在的候选源库。
pub fn find_source(app_dir: &Path) -> Option<PathBuf> {
    candidates(app_dir).into_iter().find(|p| p.is_file())
}

fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(".wpf-import.json")
}

/// 本数据目录是否已经自动导入过。
pub fn already_imported(data_dir: &Path) -> bool {
    marker_path(data_dir).is_file()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Marker {
    source: String,
    imported_ms: i64,
    inserted: usize,
    skipped_dup: usize,
}

/// 一次导入的结果。
pub struct Outcome {
    pub source: PathBuf,
    pub inserted: usize,
    pub skipped_dup: usize,
    pub bad: usize,
    pub total: i64,
    /// 同步抬高的容量（与 WPF 设置一致时抬高）；None=无需改
    pub limits: Option<(i64, i64)>,
}

/// 同步执行一次导入（CLI 与首启自动导入共用）。
///
/// 容量跟随 WPF：老库 MaxItems/MaxImageItems 大于 clipx 当前值时抬到同档，
/// 否则迁移完第一条新采集就按 clipx 默认容量触发裁剪，把刚迁进来的历史裁掉。
pub fn run_import(
    store: &Store,
    source: &Path,
    settings_path: &Path,
    settings: &Settings,
) -> Result<Outcome> {
    let mut reader = clipx_store::wpf::BatchReader::open(source)?;
    let total = reader.total().unwrap_or(0);

    let mut new_settings = settings.clone();
    if let Some(dir) = source.parent() {
        if let Ok(json) = std::fs::read_to_string(dir.join("settings.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(max_items) = v.get("MaxItems").and_then(|x| x.as_i64()) {
                    if max_items > new_settings.max_items {
                        new_settings.max_items = max_items;
                    }
                }
                if let Some(max_img) = v.get("MaxImageItems").and_then(|x| x.as_i64()) {
                    if max_img > new_settings.max_image_items {
                        new_settings.max_image_items = max_img;
                    }
                }
            }
        }
    }
    let raised = (new_settings.max_items != settings.max_items
        || new_settings.max_image_items != settings.max_image_items)
        .then(|| (new_settings.max_items, new_settings.max_image_items));
    if raised.is_some() {
        crate::settings::save(settings_path, &new_settings)
            .context("同步容量设置失败（settings.json 写入）")?;
        store.set_limits(clipx_store::StoreLimits {
            max_items: new_settings.max_items,
            max_image_items: new_settings.max_image_items,
        });
    }

    let mut inserted = 0usize;
    let mut skipped_dup = 0usize;
    let mut bad = 0usize;
    loop {
        let batch = reader.next_batch(BATCH_ROWS, BATCH_BYTES)?;
        bad += batch.bad;
        if !batch.rows.is_empty() {
            let stats = store.import_batch(batch.rows)?;
            inserted += stats.inserted;
            skipped_dup += stats.skipped_dup;
        }
        if batch.eof {
            break;
        }
        std::thread::sleep(BATCH_GAP);
    }

    Ok(Outcome {
        source: source.to_path_buf(),
        inserted,
        skipped_dup,
        bad,
        total,
        limits: raised,
    })
}

fn save_marker(data_dir: &Path, out: &Outcome) {
    let marker = Marker {
        source: out.source.display().to_string(),
        imported_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        inserted: out.inserted,
        skipped_dup: out.skipped_dup,
    };
    if let Ok(json) = serde_json::to_string_pretty(&marker) {
        let _ = std::fs::write(marker_path(data_dir), json);
    }
}

/// 后台线程：检测 → 分批导入 → 回投结果。任何失败都只记日志，不打断启动。
pub fn spawn_auto_import(
    store: Store,
    evt_tx: Sender<AppEvt>,
    data_dir: PathBuf,
    settings_path: PathBuf,
    settings: Settings,
) {
    let spawn = std::thread::Builder::new()
        .name("clipx-wpf-import".into())
        .spawn(move || {
            if already_imported(&data_dir) {
                return;
            }
            let Some(source) = find_source(&data_dir) else {
                return;
            };
            let src_label = source.display().to_string();
            crate::win_popup::append_debug_log(
                "wpf_import.log",
                &format!("auto import start source={src_label}"),
            );
            match run_import(&store, &source, &settings_path, &settings) {
                Ok(out) => {
                    save_marker(&data_dir, &out);
                    crate::win_popup::append_debug_log(
                        "wpf_import.log",
                        &format!(
                            "auto import done total={} inserted={} dup={} bad={}",
                            out.total, out.inserted, out.skipped_dup, out.bad
                        ),
                    );
                    let _ = evt_tx.send(AppEvt::WpfImported {
                        inserted: out.inserted,
                        skipped_dup: out.skipped_dup,
                        bad: out.bad,
                        source: src_label,
                        raised: out.limits,
                    });
                }
                Err(e) => {
                    crate::win_popup::append_debug_log(
                        "wpf_import.log",
                        &format!("auto import failed: {e:#}"),
                    );
                    eprintln!("WPF 历史自动导入失败: {e:#}");
                    let _ = evt_tx.send(AppEvt::WpfImportFailed(format!("{e:#}")));
                }
            }
        });
    if let Err(e) = spawn {
        eprintln!("启动 WPF 导入线程失败: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_cover_install_portable_and_dev_layouts() {
        // 用**平台原生分隔符**拼路径：Windows 字面量 `C:\app\clipx` 在 Unix 上反斜杠不算
        // 分隔符，`parent()` 会返回空串，断言就假失败（v0.10.5 的 mac/Linux CI 正是这么挂的）。
        let app = std::path::Path::new("app-root").join("clipx");
        let list = candidates(&app);
        assert!(
            list.contains(&app.join("Data").join("clipboard_history.db")),
            "便携对便携：与 clipx 同级 Data"
        );
        assert!(
            list.contains(
                &app.parent()
                    .expect("app 目录有父级")
                    .join("clipboard")
                    .join("Data")
                    .join("clipboard_history.db")
            ),
            "开发机：../clipboard/Data"
        );
        #[cfg(windows)]
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            assert!(
                list.contains(&local.join("ClipboardX").join("clipboard_history.db")),
                "WPF 安装模式（本机实测就在这个位置）"
            );
        }
        let mut deduped = list.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), list.len(), "候选路径不应重复");
    }
}
