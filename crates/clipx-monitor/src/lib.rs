use anyhow::Result;
use clipx_core::event::ClipEvent;
use clipx_core::ClipboardGate;
use std::sync::mpsc::Sender;

pub fn spawn(tx: Sender<ClipEvent>, gate: ClipboardGate) -> Result<()> {
    #[cfg(windows)]
    {
        platform::spawn(tx, gate)
    }
    #[cfg(not(windows))]
    {
        let _ = (tx, gate);
        anyhow::bail!("剪贴板监 M0/M1 仅实现 Windows；macOS/Linux 分别在 M6/M7 落地")
    }
}

#[cfg(windows)]
mod platform {
    use clipboard_rs::{common::RustImage, Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext};
    use clipx_core::event::ClipEvent;
    use clipx_core::{now_ms, ClipboardGate};
    use std::sync::mpsc::Sender;

    /// 单图 PNG 体积上限（WPF 版 MaxImageSizeBytes = 15MB，超限整体跳过不入库）
    const MAX_IMAGE_BYTES: usize = 15 * 1024 * 1024;

    struct Forwarder {
        reader: ClipboardContext,
        tx: Sender<ClipEvent>,
        gate: ClipboardGate,
    }

    impl ClipboardHandler for Forwarder {
        fn on_clipboard_change(&mut self) {
            if self.gate.should_suppress(now_ms()) {
                return;
            }
            // 文本优先（对齐 WPF 采集次序）；空文本再尝试图片
            if let Ok(text) = self.reader.get_text() {
                if !text.trim().is_empty() {
                    let _ = self.tx.send(ClipEvent::Text(text));
                    return;
                }
            }
            if let Ok(img) = self.reader.get_image() {
                if img.is_empty() {
                    return;
                }
                let (w, h) = img.get_size();
                if let Ok(png) = img.to_png() {
                    let bytes = png.get_bytes().to_vec();
                    // 即用即释：解码产物立即丢弃，只保留 PNG 字节过 channel
                    drop(img);
                    if !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES {
                        let _ = self
                            .tx
                            .send(ClipEvent::Image { blob: bytes, width: w, height: h, mime: "image/png".into() });
                    }
                }
            }
        }
    }

    pub fn spawn(tx: Sender<ClipEvent>, gate: ClipboardGate) -> anyhow::Result<()> {
        let reader =
            ClipboardContext::new().map_err(|e| anyhow::anyhow!("ClipboardContext: {e}"))?;
        let mut watcher = ClipboardWatcherContext::new()
            .map_err(|e| anyhow::anyhow!("ClipboardWatcherContext: {e}"))?;
        watcher.add_handler(Forwarder { reader, tx, gate });
        std::thread::Builder::new()
            .name("clipx-monitor".into())
            .spawn(move || {
                let _ = watcher.start_watch();
            })
            .map_err(|e| anyhow::anyhow!("启动监听线程: {e}"))?;
        Ok(())
    }
}
