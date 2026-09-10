//! 预览中尺寸渲染图（Tier2）的磁盘缓存 + 后台生成 worker。
//!
//! 三级渐进预览：64px 缩略图（瞬时，手头已有）→ 1280 JPEG 渲染图（小文件快解，
//! 本模块）→ 1600 全解（放大用，logic 预览缓存）。渲染图只做显示，粘贴仍走原图。
//!
//! 生成时机：入库后 worker 异步生成（不挡捕获线程）；预览 miss/邻居预取按需入队。
//! 目录：`Data/previews/{id}.jpg`（随 Data 走便携/安装双模式），上限 400 个/200MB。

use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;

use clipx_store::Store;

/// 渲染 worker 队列容量：入库突发与预取共用，满则丢（预览 miss 路径会兜底全解）。
const QUEUE_BOUND: usize = 16;
/// 磁盘缓存上限（文件数 / 总字节）。
const MAX_FILES: usize = 400;
const MAX_BYTES: u64 = 200 * 1024 * 1024;

pub fn dir_for(settings_path: &Path) -> PathBuf {
    settings_path
        .parent()
        .unwrap_or(settings_path)
        .join("previews")
}

pub fn path_for(dir: &Path, id: i64) -> PathBuf {
    dir.join(format!("{id}.jpg"))
}

/// 读渲染图（预览 Tier2 用）；缺失回 None（调用方走全解 + 落盘）。
pub fn load(dir: &Path, id: i64) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path_for(dir, id)).ok()?;
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        Some(bytes)
    } else {
        None
    }
}

/// 原子落盘（tmp + rename），失败静默（显示链路不因缓存失败中断）。
pub fn store_bytes(dir: &Path, id: i64, jpg: &[u8]) {
    if jpg.is_empty() {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let dst = path_for(dir, id);
    let tmp = dst.with_extension("tmp");
    if std::fs::write(&tmp, jpg).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, &dst);
}

pub fn remove(dir: &Path, id: i64) {
    let _ = std::fs::remove_file(path_for(dir, id));
}

/// 启动时清上限（readdir 量级，调用方放后台线程）。
pub fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jpg") {
            continue;
        }
        let Ok(m) = e.metadata() else {
            continue;
        };
        if !m.is_file() {
            continue;
        }
        let mtime = m.modified().unwrap_or(std::time::UNIX_EPOCH);
        files.push((p, m.len(), mtime));
    }
    if files.len() <= MAX_FILES && files.iter().map(|(_, n, _)| n).sum::<u64>() <= MAX_BYTES {
        return;
    }
    // 最旧优先删（渲染图可再生，删了下次预览重建）。
    files.sort_by(|a, b| a.2.cmp(&b.2));
    let mut count = files.len();
    let mut bytes: u64 = files.iter().map(|(_, n, _)| n).sum();
    for (p, n, _) in &files {
        if count <= MAX_FILES && bytes <= MAX_BYTES {
            break;
        }
        if std::fs::remove_file(p).is_ok() {
            count -= 1;
            bytes = bytes.saturating_sub(*n);
        }
    }
}

#[derive(Clone)]
pub struct RenditionQueue {
    tx: SyncSender<i64>,
}

impl RenditionQueue {
    pub fn spawn(store: Store, dir: PathBuf) -> anyhow::Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<i64>(QUEUE_BOUND);
        std::thread::Builder::new()
            .name("clipx-rendition".into())
            .spawn(move || {
                #[cfg(windows)]
                unsafe {
                    use windows::Win32::System::Threading::{
                        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
                    };
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                }
                while let Ok(id) = rx.recv() {
                    if std::fs::metadata(path_for(&dir, id)).is_ok() {
                        continue;
                    }
                    let Some(row) = store.get_image(id) else {
                        continue;
                    };
                    if row.blob.is_empty() {
                        continue;
                    }
                    if let Some(jpg) = make_rendition_jpeg(&row.blob) {
                        store_bytes(&dir, id, &jpg);
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("启动渲染 worker 失败: {e}"))?;
        Ok(Self { tx })
    }

    /// 入队（满则丢：预览 miss 路径会全解兜底，下次命中）。
    pub fn request(&self, id: i64) {
        let _ = self.tx.try_send(id);
    }
}

fn make_rendition_jpeg(blob: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        if let Some(d) = crate::wic::decode_limited(blob, clipx_core::PREVIEW_RENDITION_WIDTH) {
            if let Some(jpg) = clipx_core::encode_preview_jpeg_rgba(&d.rgba, d.w, d.h) {
                return Some(jpg);
            }
        }
    }
    clipx_core::make_preview_rendition(blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clipx-rendition-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn store_load_roundtrip_and_prune() {
        let d = tmp_dir("roundtrip");
        assert!(load(&d, 1).is_none());
        store_bytes(&d, 1, &[0xFF, 0xD8, 0xFF, 0xE0]);
        assert_eq!(load(&d, 1), Some(vec![0xFF, 0xD8, 0xFF, 0xE0]));
        store_bytes(&d, 2, &[1, 2, 3]);
        assert!(load(&d, 2).is_none(), "非 JPEG 拒绝");
        remove(&d, 1);
        assert!(load(&d, 1).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
