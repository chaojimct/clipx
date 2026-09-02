//! OCR 封装：引擎 trait（跨平台切换点）+ 单工作线程有界队列。
//!
//! 线程约定（ARCHITECTURE §3）：OCR 工作线程从队列取条目 id → store 懒加载原图
//! → 引擎识别（图片 bytes 用完即弃）→ ocr_text 回填 store → 通知 UI 刷新。
//! 队列有界（64），满则丢弃未入队项（下次启动 backfill 补做），不允许堆积。

pub mod postprocess;

#[cfg(windows)]
mod media_ocr;
#[cfg(windows)]
pub use media_ocr::MediaOcrEngine;

use std::sync::mpsc::{Receiver, SyncSender};

use clipx_store::Store;

/// 识别引擎抽象（uniOCR 角色，ADR-004 的直调替换路径：接口不变换实现）。
pub trait OcrEngine: Send {
    /// 输入 PNG 字节，返回后处理完成的 OCR 文本（空串表示图中无文字）。
    fn recognize_png(&mut self, png: &[u8]) -> anyhow::Result<String>;
}

/// 队列容量：入队与启动回填共用该上限
pub const QUEUE_BOUND: usize = 64;

#[derive(Clone)]
pub struct OcrQueue {
    tx: SyncSender<i64>,
}

impl OcrQueue {
    /// 启动 OCR 工作线程。engine_factory 在工作线程上调用一次（WinRT 引擎
    /// 需与线程绑定）；返回 None 表示引擎不可用（如缺语言包），线程空转排空队列。
    pub fn spawn<F, G>(store: Store, engine_factory: F, on_event: G) -> anyhow::Result<OcrQueue>
    where
        F: FnOnce() -> Option<Box<dyn OcrEngine>> + Send + 'static,
        G: Fn() + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<i64>(QUEUE_BOUND);
        std::thread::Builder::new()
            .name("clipx-ocr".into())
            .spawn(move || worker(rx, store, engine_factory, on_event))
            .map_err(|e| anyhow::anyhow!("启动 OCR 工作线程失败: {e}"))?;
        Ok(OcrQueue { tx })
    }

    /// 入队一个条目；队列满时丢弃（该条保持未处理状态，下次启动回填）。
    pub fn enqueue(&self, id: i64) {
        let _ = self.tx.try_send(id);
    }

    pub fn enqueue_many(&self, ids: &[i64]) {
        for id in ids {
            self.enqueue(*id);
        }
    }
}

fn worker<F, G>(rx: Receiver<i64>, store: Store, engine_factory: F, on_event: G)
where
    F: FnOnce() -> Option<Box<dyn OcrEngine>>,
    G: Fn(),
{
    let Some(mut engine) = engine_factory() else {
        eprintln!("OCR 引擎不可用（缺少语言包？），本次会话跳过 OCR");
        // 排空队列，避免 sender 阻塞；条目保持未处理状态
        while rx.try_recv().is_ok() {}
        return;
    };
    let mut process = |id: i64| {
        // 已完成的跳过（启动回填与新采集可能重复入队）
        if store.get_ocr(id).map(|o| o.state) == Some(2) {
            return;
        }
        store.mark_ocr_pending(id);
        let Some(image) = store.get_image(id) else {
            // 负载缺失：标记终态，否则自驱回填循环会反复拉到同一批
            store.mark_ocr_failed(id);
            return;
        };
        match engine.recognize_png(&image.blob) {
            Ok(text) => store.set_ocr_text(id, text),
            Err(e) => {
                eprintln!("OCR 识别失败 (id={id}): {e:#}");
                store.mark_ocr_failed(id);
            }
        }
        // 即用即释：image（原图 bytes）在循环体作用域结束即释放
        on_event();
    };
    // 启动回填（迁移图片的 OCR 补做）：worker 自己批量拉取直到清空。
    // 通道同步排空，避免与新采集条目互相饿死；每条处理后即落终态（2/3），
    // 下一批查询自然收敛，不会重复。
    loop {
        while let Ok(id) = rx.try_recv() {
            process(id);
        }
        let batch = store.list_ocr_backfill(QUEUE_BOUND as i64);
        if batch.is_empty() {
            break;
        }
        for id in batch {
            process(id);
        }
    }
    while let Ok(id) = rx.recv() {
        process(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipx_core::NewEntry;
    use clipx_store::{InsertOutcome, StoreLimits};

    struct MockEngine {
        reply: String,
    }

    impl OcrEngine for MockEngine {
        fn recognize_png(&mut self, _png: &[u8]) -> anyhow::Result<String> {
            Ok(self.reply.clone())
        }
    }

    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::new(w, h);
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn queue_runs_ocr_and_stores_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("clipx.db"), StoreLimits::default()).unwrap();
        let outcome = store
            .insert(NewEntry::from_image(
                tiny_png(20, 20),
                20,
                20,
                "image/png".into(),
            ))
            .unwrap();
        let InsertOutcome::Inserted(id) = outcome else {
            panic!()
        };

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let queue = OcrQueue::spawn(
            store.clone(),
            || {
                Some(Box::new(MockEngine {
                    reply: "你好 世界".into(),
                }))
            },
            move || {
                let _ = done_tx.send(());
            },
        )
        .unwrap();

        queue.enqueue(id);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("OCR 未在限时内完成");

        let ocr = store.get_ocr(id).unwrap();
        assert_eq!(ocr.state, 2);
        assert_eq!(ocr.text.as_deref(), Some("你好 世界"));

        // 重复入队已完成条目：worker 直接跳过，不再回调
        let (done_tx2, done_rx2) = std::sync::mpsc::channel::<()>();
        let _ = done_tx2;
        drop(done_rx2);
        queue.enqueue(id);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(store.get_ocr(id).unwrap().state, 2);
    }
}
