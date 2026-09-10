//! macOS 剪贴板采集：无系统事件 API，clipboard-rs watcher 内部轮询
//! `NSPasteboard.changeCount`（500ms，ADR-002；Maccy/PasteBar 同法）。
//!
//! 采集次序对齐 Windows 版：文件 > 富文本 > 纯文本 > 图片。
//! mac 无 CF_HDROP 概念，文件列表走 `public.file-url`（clipboard-rs get_files）。

use std::sync::mpsc::Sender;
use std::time::Duration;

use clipboard_rs::common::RustImage;
use clipboard_rs::{
    Clipboard, ClipboardHandler, ClipboardContext, ClipboardWatcher, ClipboardWatcherContext,
    ContentFormat,
};
use clipx_core::event::ClipEvent;
use clipx_core::{now_ms, ClipboardGate};

/// 单图 PNG 体积上限（顶层原子量热更新）。
fn max_image_bytes() -> usize {
    super::MAX_IMAGE_BYTES.load(std::sync::atomic::Ordering::SeqCst)
}

pub struct Forwarder {
    reader: ClipboardContext,
    tx: Sender<ClipEvent>,
    gate: ClipboardGate,
}

impl ClipboardHandler for Forwarder {
    fn on_clipboard_change(&mut self) {
        if self.gate.should_suppress(now_ms()) {
            return;
        }
        // 文件：Finder 拷贝文件产生 public.file-url
        if let Ok(files) = self.reader.get_files() {
            if !files.is_empty() {
                let _ = self.tx.send(ClipEvent::Files(files));
                return;
            }
        }
        // 富文本：mac 剪贴板 HTML 直接是 text/html（无 Windows 注册格式）
        let text = self.reader.get_text().unwrap_or_default();
        let html = self.reader.get_html().unwrap_or_default();
        if !html.is_empty() && !text.is_empty() {
            let _ = self.tx.send(ClipEvent::RichText { text, html });
            return;
        }
        if !text.is_empty() {
            let _ = self.tx.send(ClipEvent::Text(text));
            return;
        }
        if self.reader.has(ContentFormat::Image) {
            self.read_image();
        }
    }
}

impl Forwarder {
    /// 图片解码走 clipboard-rs（NSPasteboard TIFF/PNG 多格式），
    /// 缩略图 + 预览 JPEG 从已解码位图压出（与 Windows 相同管线）。
    fn read_image(&self) {
        let Ok(img) = self.reader.get_image() else {
            return;
        };
        if img.is_empty() {
            return;
        }
        let (w, h) = img.get_size();
        let deriv = img
            .thumbnail(
                clipx_core::PREVIEW_RENDITION_WIDTH,
                clipx_core::PREVIEW_RENDITION_WIDTH,
            )
            .ok()
            .and_then(|m| m.get_dynamic_image().ok())
            .map(clipx_core::make_image_derivatives);
        let Ok(png) = img.to_png() else {
            return;
        };
        let bytes = png.get_bytes().to_vec();
        drop(img);
        if bytes.is_empty() || bytes.len() > max_image_bytes() {
            return;
        }
        let (thumb, thumb_w, thumb_h, rendition_jpeg) = match deriv {
            Some(d) => (d.thumb, d.thumb_w, d.thumb_h, d.rendition_jpeg),
            None => (Vec::new(), 0, 0, Vec::new()),
        };
        let _ = self.tx.send(ClipEvent::Image {
            blob: bytes,
            width: w,
            height: h,
            mime: "image/png".into(),
            thumb,
            thumb_w,
            thumb_h,
            rendition_jpeg,
        });
    }
}

pub fn spawn(tx: Sender<ClipEvent>, gate: ClipboardGate) -> anyhow::Result<()> {
    let reader = ClipboardContext::new().map_err(|e| anyhow::anyhow!("ClipboardContext: {e}"))?;
    let mut watcher = ClipboardWatcherContext::new()
        .map_err(|e| anyhow::anyhow!("ClipboardWatcherContext: {e}"))?;
    watcher.add_handler(Forwarder { reader, tx, gate });
    std::thread::Builder::new()
        .name("clipx-monitor".into())
        .spawn(move || {
            watcher.start_watch();
        })
        .map_err(|e| anyhow::anyhow!("启动监听线程: {e}"))?;
    Ok(())
}
