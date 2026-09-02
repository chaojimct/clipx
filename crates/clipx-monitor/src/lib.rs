use anyhow::Result;
use clipx_core::event::ClipEvent;
use std::sync::mpsc::Sender;

pub fn spawn(tx: Sender<ClipEvent>) -> Result<()> {
    #[cfg(windows)]
    {
        platform::spawn(tx)
    }
    #[cfg(not(windows))]
    {
        let _ = tx;
        anyhow::bail!("剪贴板监听 M0 仅实现 Windows；macOS/Linux 分别在 M6/M7 落地")
    }
}

#[cfg(windows)]
mod platform {
    use clipboard_rs::{Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext};
    use clipx_core::event::ClipEvent;
    use std::sync::mpsc::Sender;

    struct Forwarder {
        reader: ClipboardContext,
        tx: Sender<ClipEvent>,
    }

    impl ClipboardHandler for Forwarder {
        fn on_clipboard_change(&mut self) {
            let Ok(text) = self.reader.get_text() else { return };
            if !text.trim().is_empty() {
                let _ = self.tx.send(ClipEvent::Text(text));
            }
        }
    }

    pub fn spawn(tx: Sender<ClipEvent>) -> anyhow::Result<()> {
        let reader =
            ClipboardContext::new().map_err(|e| anyhow::anyhow!("ClipboardContext: {e}"))?;
        let mut watcher = ClipboardWatcherContext::new()
            .map_err(|e| anyhow::anyhow!("ClipboardWatcherContext: {e}"))?;
        watcher.add_handler(Forwarder { reader, tx });
        std::thread::Builder::new()
            .name("clipx-monitor".into())
            .spawn(move || {
                let _ = watcher.start_watch();
            })
            .map_err(|e| anyhow::anyhow!("启动监听线程: {e}"))?;
        Ok(())
    }
}
