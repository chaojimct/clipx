//! OCR 封装：引擎 trait（跨平台切换点）+ 单工作线程有界队列。
//!
//! 线程约定（ARCHITECTURE §3）：OCR 工作线程从队列取条目 id → store 懒加载原图
//! → 引擎识别（图片 bytes 用完即弃）→ ocr_text 回填 store → 通知 UI 刷新。
//! 队列有界（64），满则丢弃未入队项（下次启动 backfill 补做），不允许堆积。

pub mod postprocess;
pub mod rapid;

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

    /// 带行框的识别（P1a）：文本与 `recognize_png` 一致，另附行级框
    /// （归一化 0-1 坐标，相对送入引擎的缩放后位图）。
    /// 默认实现只给文本、行框为空（旧引擎/单测 Mock 走此路径）。
    fn recognize_lines(&mut self, png: &[u8]) -> anyhow::Result<OcrDetailed> {
        Ok(OcrDetailed {
            text: self.recognize_png(png)?,
            lines: Vec::new(),
        })
    }
}

/// OCR 行框：文本 + 归一化外接框（相对识别用位图，0-1）。
/// 存库时整体序列化为 JSON（payloads.ocr_boxes），每行约 30-60 字节。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OcrLine {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 词级框（P1b-2）：中文引擎词多为 1-3 字，约等于按字选。
    /// 缺失（P1a 旧数据）时反序列化默认为空，预览降级为行框。
    #[serde(default)]
    pub words: Vec<OcrWord>,
}

/// OCR 词框：归一化坐标（相对识别用位图，0-1）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OcrWord {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// 带框识别结果：全文（后处理后）+ 行框。
#[derive(Debug, Clone)]
pub struct OcrDetailed {
    pub text: String,
    pub lines: Vec<OcrLine>,
}

impl OcrLine {
    /// 归一化钳制（引擎浮点误差可能微超 0-1）。
    pub fn clamped(mut self) -> Self {
        self.x = self.x.clamp(0.0, 1.0);
        self.y = self.y.clamp(0.0, 1.0);
        self.w = self.w.clamp(0.0, 1.0 - self.x);
        self.h = self.h.clamp(0.0, 1.0 - self.y);
        self.words = self.words.into_iter().map(OcrWord::clamped).collect();
        self
    }
}

impl OcrWord {
    pub fn clamped(mut self) -> Self {
        self.x = self.x.clamp(0.0, 1.0);
        self.y = self.y.clamp(0.0, 1.0);
        self.w = self.w.clamp(0.0, 1.0 - self.x);
        self.h = self.h.clamp(0.0, 1.0 - self.y);
        self
    }
}

/// 队列容量：入队与启动回填共用该上限
pub const QUEUE_BOUND: usize = 64;

/// OCR 引擎选择（设置 `ocr_engine`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OcrEngineMode {
    /// 有拓展包用拓展包，否则系统引擎（无系统引擎则无 OCR）。
    Auto,
    /// 只用系统引擎（Windows Media OCR）。
    Media,
    /// 只用拓展包（模型缺失时本任务回退系统引擎）。
    Rapid,
}

impl OcrEngineMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "media" | "system" | "系统" => Self::Media,
            "rapid" | "pack" | "拓展包" | "高精度" => Self::Rapid,
            _ => Self::Auto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Media => "media",
            Self::Rapid => "rapid",
        }
    }
}

/// 自动调度引擎：拓展包优先（按任务新建 session，用完即弃），
/// 失败/缺失回退系统引擎。模式切换需重启（引擎在队列线程持有）。
pub struct AutoOcrEngine {
    mode: OcrEngineMode,
    model_dir: std::path::PathBuf,
    #[cfg(windows)]
    media: Option<MediaOcrEngine>,
}

impl AutoOcrEngine {
    /// 在 OCR 工作线程上调用（MediaOcrEngine 与线程绑定）。
    pub fn new(mode: OcrEngineMode, model_dir: std::path::PathBuf) -> Self {
        #[cfg(windows)]
        let media = MediaOcrEngine::new();
        Self {
            mode,
            model_dir,
            #[cfg(windows)]
            media,
        }
    }

    /// 本次任务是否走拓展包（feature + 模式 + 模型齐）。
    fn use_rapid(&self) -> bool {
        #[cfg(feature = "rapid")]
        {
            matches!(self.mode, OcrEngineMode::Auto | OcrEngineMode::Rapid)
                && rapid::models_ready(&self.model_dir)
        }
        #[cfg(not(feature = "rapid"))]
        {
            // 无 feature 永远 false；读字段免 dead_code（构造 API 保持稳定）。
            let _ = (&self.mode, &self.model_dir);
            false
        }
    }

    fn run_detailed(&mut self, png: &[u8]) -> anyhow::Result<OcrDetailed> {
        #[cfg(feature = "rapid")]
        if self.use_rapid() {
            match rapid::RapidOcrEngine::new(&self.model_dir)
                .and_then(|mut e| e.recognize_lines(png))
            {
                Ok(d) => return Ok(d),
                Err(e) => eprintln!("RapidOCR 失败，回退系统引擎: {e:#}"),
            }
        }
        #[cfg(windows)]
        if let Some(m) = self.media.as_mut() {
            // Media 路径自带行框（P1a），保持原行为。
            return m.recognize_lines(png);
        }
        anyhow::bail!("OCR 引擎不可用")
    }
}

impl OcrEngine for AutoOcrEngine {
    fn recognize_png(&mut self, png: &[u8]) -> anyhow::Result<String> {
        Ok(self.run_detailed(png)?.text)
    }

    fn recognize_lines(&mut self, png: &[u8]) -> anyhow::Result<OcrDetailed> {
        self.run_detailed(png)
    }
}

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
        // 已完成的跳过（启动回填与新采集可能重复入队）。
        // P1a：state=2 但框为 NULL 的是 v7 前旧行，需补框（一次，回填后收敛）。
        if let Some(o) = store.get_ocr(id) {
            if o.state == 2 && o.boxes.is_some() {
                return;
            }
        }
        store.mark_ocr_pending(id);
        let Some(image) = store.get_image(id) else {
            // 负载缺失：标记终态，否则自驱回填循环会反复拉到同一批
            store.mark_ocr_failed(id);
            return;
        };
        match engine.recognize_lines(&image.blob) {
            Ok(detailed) => {
                // 空行存 "[]"（已处理标记，与 NULL=未处理区分）。
                let boxes_json =
                    serde_json::to_string(&detailed.lines).unwrap_or_else(|_| "[]".into());
                store.set_ocr_result(id, detailed.text, boxes_json);
            }
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

    #[test]
    fn engine_mode_parse_and_roundtrip() {
        assert_eq!(OcrEngineMode::parse("auto"), OcrEngineMode::Auto);
        assert_eq!(OcrEngineMode::parse(""), OcrEngineMode::Auto);
        assert_eq!(OcrEngineMode::parse("未知"), OcrEngineMode::Auto);
        assert_eq!(OcrEngineMode::parse("media"), OcrEngineMode::Media);
        assert_eq!(OcrEngineMode::parse("system"), OcrEngineMode::Media);
        assert_eq!(OcrEngineMode::parse("rapid"), OcrEngineMode::Rapid);
        assert_eq!(OcrEngineMode::parse("拓展包"), OcrEngineMode::Rapid);
        assert_eq!(OcrEngineMode::Rapid.as_str(), "rapid");
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
    fn queue_runs_ocr_and_stores_text() {        let dir = tempfile::tempdir().unwrap();
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
        // Mock 只实现 recognize_png：默认 recognize_lines 给空行，落 "[]" 标记
        assert_eq!(ocr.boxes.as_deref(), Some("[]"));
        assert!(store.list_ocr_backfill(100).is_empty());

        // 重复入队已完成条目：worker 直接跳过，不再回调
        let (done_tx2, done_rx2) = std::sync::mpsc::channel::<()>();
        let _ = done_tx2;
        drop(done_rx2);
        queue.enqueue(id);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(store.get_ocr(id).unwrap().state, 2);
    }

    /// 带框引擎：worker 把行框 JSON 落库（P1a 链路）。
    struct MockLinesEngine;

    impl OcrEngine for MockLinesEngine {
        fn recognize_png(&mut self, _png: &[u8]) -> anyhow::Result<String> {
            Ok("ab".into())
        }

        fn recognize_lines(&mut self, _png: &[u8]) -> anyhow::Result<OcrDetailed> {
            Ok(OcrDetailed {
                text: "ab".into(),
                lines: vec![OcrLine {
                    text: "ab".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 0.5,
                    words: vec![
                        OcrWord {
                            text: "a".into(),
                            x: 0.0,
                            y: 0.0,
                            w: 0.5,
                            h: 0.5,
                        },
                        OcrWord {
                            text: "b".into(),
                            x: 0.5,
                            y: 0.0,
                            w: 0.5,
                            h: 0.5,
                        },
                    ],
                }],
            })
        }
    }

    #[test]
    fn queue_stores_ocr_boxes() {
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
        let queue = OcrQueue::spawn(store.clone(), || Some(Box::new(MockLinesEngine)), move || {
            let _ = done_tx.send(());
        })
        .unwrap();

        queue.enqueue(id);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("OCR 未在限时内完成");

        let ocr = store.get_ocr(id).unwrap();
        assert_eq!(ocr.text.as_deref(), Some("ab"));
        let boxes = ocr.boxes.unwrap_or_default();
        assert!(boxes.contains("\"text\":\"ab\""), "{boxes}");
        assert!(boxes.contains("\"words\""), "{boxes}");
        assert!(store.list_ocr_backfill(100).is_empty());
    }

    /// 旧行框 JSON（无 words 字段）向前兼容：反序列化出空词表。
    #[test]
    fn legacy_line_json_without_words_parses() {
        let lines: Vec<OcrLine> =
            serde_json::from_str(r#"[{"text":"ab","x":0.0,"y":0.0,"w":1.0,"h":0.5}]"#)
                .unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].words.is_empty());
        // 新格式往返
        let back = serde_json::to_string(&lines).unwrap();
        let again: Vec<OcrLine> = serde_json::from_str(&back).unwrap();
        assert_eq!(again[0].text, "ab");
    }
}
