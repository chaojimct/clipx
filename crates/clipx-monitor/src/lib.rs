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
    use clipboard_rs::{Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext};
    use clipx_core::event::ClipEvent;
    use clipx_core::{now_ms, ClipboardGate};
    use std::sync::mpsc::Sender;

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
            let Ok(text) = self.reader.get_text() else { return };
            if !text.trim().is_empty() {
                let _ = self.tx.send(ClipEvent::Text(text));
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
