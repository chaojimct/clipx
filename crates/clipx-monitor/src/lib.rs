use anyhow::Result;
use clipx_core::event::ClipEvent;
use clipx_core::ClipboardGate;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

/// 单图 PNG 体积上限（设置热更新；默认对齐 WPF 15MB）
static MAX_IMAGE_BYTES: AtomicUsize = AtomicUsize::new(15 * 1024 * 1024);

pub fn set_max_image_bytes(n: u64) {
    let n = n.clamp(1024, 200 * 1024 * 1024) as usize;
    MAX_IMAGE_BYTES.store(n, Ordering::SeqCst);
}

pub fn spawn(tx: Sender<ClipEvent>, gate: ClipboardGate) -> Result<()> {
    #[cfg(windows)]
    {
        platform::spawn(tx, gate)
    }
    #[cfg(target_os = "macos")]
    {
        platform::spawn(tx, gate)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (tx, gate);
        anyhow::bail!("剪贴板监听：Linux 在 M7 落地（X11 / wl-clipboard）")
    }
}

/// macOS 采集：独立文件，与 Windows `platform` 同接口（spawn）。
#[cfg(target_os = "macos")]
#[path = "platform_macos.rs"]
pub(crate) mod platform;

#[cfg(windows)]
pub(crate) mod platform {
    use clipboard_rs::{
        common::RustImage, Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher,
        ClipboardWatcherContext,
    };
    use clipx_core::event::ClipEvent;
    use clipx_core::{now_ms, ClipboardGate};
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    /// 单图 PNG 体积上限（由 `set_max_image_bytes` 热更新）
    fn max_image_bytes() -> usize {
        super::MAX_IMAGE_BYTES.load(std::sync::atomic::Ordering::SeqCst)
    }

    // 标准剪贴板格式 id（Win32 固有值，直接用常量避免多余 feature）
    const CF_DIB: u32 = 2;
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP: u32 = 15;
    const CF_DIBV5: u32 = 17;

    struct Forwarder {
        reader: ClipboardContext,
        html_format: u32,
        png_format: u32,
        tx: Sender<ClipEvent>,
        gate: ClipboardGate,
    }

    impl ClipboardHandler for Forwarder {
        fn on_clipboard_change(&mut self) {
            if self.gate.should_suppress(now_ms()) {
                return;
            }
            // 原子快照：一次 OpenClipboard 内判定并读取全部候选格式。
            // 逐格式独立开合（clipboard-rs 默认路径）在变更瞬间会与 rdpclip
            // 等监听方竞争 open，弱重试导致类型误判（如有 HTML 却采成纯文本）。
            match snapshot::read(self.html_format, self.png_format) {
                Some(snapshot::Kind::Files(paths)) => {
                    let _ = self.tx.send(ClipEvent::Files(paths));
                }
                // 采集次序对齐 WPF 版：文件 > 富文本 > 纯文本 > 图片
                Some(snapshot::Kind::RichText { text, html }) => {
                    let _ = self.tx.send(ClipEvent::RichText { text, html });
                }
                Some(snapshot::Kind::Text(text)) => {
                    if std::env::var_os("CLIPX_DIAG").is_some() {
                        eprintln!("[diag] event -> Text({} 字)", text.chars().count());
                    }
                    let _ = self.tx.send(ClipEvent::Text(text));
                }
                Some(snapshot::Kind::Image) => self.read_image(),
                Some(snapshot::Kind::Empty) => {
                    if std::env::var_os("CLIPX_DIAG").is_some() {
                        eprintln!("[diag] event -> Empty");
                    }
                }
                None => {
                    if std::env::var_os("CLIPX_DIAG").is_some() {
                        eprintln!("[diag] event -> read abandoned (open contention)");
                    }
                }
            }
        }
    }

    impl Forwarder {
        /// 图片解码走 clipboard-rs（注册 PNG → CF_DIBV5 → CF_DIB 多级回退）。
        /// 其内部 open 重试是 Sleep(0)×10 的微秒级自旋，外层补真实退避。
        fn read_image(&self) {
            for _ in 0..10 {
                if let Ok(img) = self.reader.get_image() {
                    if img.is_empty() {
                        return;
                    }
                    let (w, h) = img.get_size();
                    // 缩略图 + 预览 JPEG 从已解码位图压出（先缩到 1280 再编，不解 PNG）。
                    let deriv = img
                        .thumbnail(
                            clipx_core::PREVIEW_RENDITION_WIDTH,
                            clipx_core::PREVIEW_RENDITION_WIDTH,
                        )
                        .ok()
                        .and_then(|m| m.get_dynamic_image().ok())
                        .map(clipx_core::make_image_derivatives);
                    if let Ok(png) = img.to_png() {
                        let bytes = png.get_bytes().to_vec();
                        drop(img);
                        if !bytes.is_empty() && bytes.len() <= max_image_bytes() {
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
                    return;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        }
    }

    pub fn spawn(tx: Sender<ClipEvent>, gate: ClipboardGate) -> anyhow::Result<()> {
        let reader =
            ClipboardContext::new().map_err(|e| anyhow::anyhow!("ClipboardContext: {e}"))?;
        let mut watcher = ClipboardWatcherContext::new()
            .map_err(|e| anyhow::anyhow!("ClipboardWatcherContext: {e}"))?;
        watcher.add_handler(Forwarder {
            reader,
            html_format: snapshot::register_format("HTML Format"),
            png_format: snapshot::register_format("PNG"),
            tx,
            gate,
        });
        std::thread::Builder::new()
            .name("clipx-monitor".into())
            .spawn(move || {
                watcher.start_watch();
            })
            .map_err(|e| anyhow::anyhow!("启动监听线程: {e}"))?;
        Ok(())
    }

    /// 单次打开的剪贴板快照（类型判定与读取在同一 open 周期内完成）。
    pub(crate) mod snapshot {
        use std::time::Duration;
        use windows::core::HSTRING;
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
            RegisterClipboardFormatW,
        };
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

        use super::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};

        /// 变更瞬间剪贴板常被并发监听方（rdpclip 等）短暂占用；持有方还可能被
        /// 粗暴 CloseClipboard 打断（实测约 1% 概率，GetClipboardData 报
        /// ERROR_CLIPBOARD_NOT_OPEN）。整体重试打开+读取，几次失败才放弃本次
        /// 事件（下次变更再采）。
        const READ_ATTEMPTS: usize = 20;
        const READ_RETRY_MS: u64 = 15;

        #[derive(Debug)]
        pub enum Kind {
            Files(Vec<String>),
            Text(String),
            RichText { text: String, html: String },
            Image,
            Empty,
        }

        pub fn register_format(name: &str) -> u32 {
            unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) }
        }

        pub fn read(html_format: u32, png_format: u32) -> Option<Kind> {
            for attempt in 0..READ_ATTEMPTS {
                let kind = unsafe {
                    let mut k = None;
                    if OpenClipboard(None).is_ok() {
                        k = read_open(html_format, png_format);
                        let _ = CloseClipboard();
                    }
                    k
                };
                if std::env::var_os("CLIPX_DIAG").is_some() {
                    eprintln!("[diag] snapshot attempt {attempt}: {kind:?}");
                }
                match kind {
                    Some(kind) => return Some(kind),
                    // open 失败或读取中途被打断：退避后整体重试
                    None if attempt + 1 < READ_ATTEMPTS => {
                        std::thread::sleep(Duration::from_millis(READ_RETRY_MS))
                    }
                    None => return None,
                }
            }
            unreachable!()
        }

        /// None = 读取中途失败（open 被打断），需要整体重试；
        /// Some(Kind::Empty) = 剪贴板确实无可用格式。
        unsafe fn read_open(html_format: u32, png_format: u32) -> Option<Kind> {
            if is_avail(CF_HDROP) {
                return match read_files() {
                    Some(paths) if !paths.is_empty() => Some(Kind::Files(paths)),
                    Some(_) => Some(Kind::Empty),
                    None => None,
                };
            }
            if is_avail(CF_UNICODETEXT) {
                let text = read_text()?;
                if text.trim().is_empty() {
                    return Some(Kind::Empty);
                }
                // 富文本：文本与 HTML Format 同时在场（浏览器/VS Code/Word 复制）
                if is_avail(html_format) {
                    match read_html(html_format) {
                        Some(html) if !html.trim().is_empty() => {
                            return Some(Kind::RichText { text, html })
                        }
                        // HTML 在场但内容为空 → 按纯文本处理
                        Some(_) => {}
                        // 读取失败 → 不可降级成纯文本（会误存 kind=0），整体重试
                        None => return None,
                    }
                }
                return Some(Kind::Text(text));
            }
            if is_avail(png_format) || is_avail(CF_DIB) || is_avail(CF_DIBV5) {
                return Some(Kind::Image);
            }
            Some(Kind::Empty)
        }

        unsafe fn is_avail(fmt: u32) -> bool {
            IsClipboardFormatAvailable(fmt).is_ok()
        }

        unsafe fn hglobal(fmt: u32) -> Option<HGLOBAL> {
            let handle = GetClipboardData(fmt).ok()?;
            // 格式数据以 HGLOBAL 传递（本工具读取的四种格式全部如此）
            Some(HGLOBAL(handle.0))
        }

        /// CF_HDROP 文件列表 → 路径集合
        unsafe fn read_files() -> Option<Vec<String>> {
            let handle = GetClipboardData(CF_HDROP).ok()?;
            let hdrop = HDROP(handle.0);
            let count = DragQueryFileW(hdrop, u32::MAX, None);
            let mut paths = Vec::with_capacity(count as usize);
            for i in 0..count {
                // 先查长度（不含 NUL）再取内容
                let len = DragQueryFileW(hdrop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                if DragQueryFileW(hdrop, i, Some(&mut buf)) > 0 {
                    paths.push(String::from_utf16_lossy(&buf[..len]));
                }
            }
            Some(paths)
        }

        unsafe fn read_text() -> Option<String> {
            let h = hglobal(CF_UNICODETEXT)?;
            let ptr = GlobalLock(h) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let len = GlobalSize(h) / 2;
            let mut text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(h);
            while text.ends_with('\0') {
                text.pop();
            }
            Some(text)
        }

        unsafe fn read_html(html_format: u32) -> Option<String> {
            let h = hglobal(html_format)?;
            let ptr = GlobalLock(h) as *const u8;
            if ptr.is_null() {
                return None;
            }
            let len = GlobalSize(h);
            let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
            let _ = GlobalUnlock(h);
            Some(extract_html(&bytes))
        }

        /// CF_HTML 头部解析：偏移是字节偏移，先在字节层切片再转字符串。
        /// 优先 StartHTML/EndHTML，退化 StartFragment/EndFragment（部分应用
        /// 只写 fragment 偏移），再退化为剥掉头部行——比 clipboard-rs 只认
        /// StartHTML/EndHTML 的实现更宽容。
        fn extract_html(bytes: &[u8]) -> String {
            let (start, end) = cf_html_range(bytes);
            let end = end.min(bytes.len());
            if start >= end {
                return String::new();
            }
            String::from_utf8_lossy(&bytes[start..end])
                .trim()
                .to_string()
        }

        pub(crate) fn cf_html_range(bytes: &[u8]) -> (usize, usize) {
            let mut start_html = None;
            let mut end_html = None;
            let mut start_frag = None;
            let mut end_frag = None;
            let mut header_end = 0usize;
            let lines = split_lines(bytes);
            for (idx, &(ls, le)) in lines.iter().enumerate() {
                let line = String::from_utf8_lossy(&bytes[ls..le]);
                let Some((key, value)) = line.split_once(':') else {
                    break;
                };
                let (key, value) = (key.trim(), value.trim());
                match key {
                    "StartHTML" => start_html = value.parse().ok(),
                    "EndHTML" => end_html = value.parse().ok(),
                    "StartFragment" => start_frag = value.parse().ok(),
                    "EndFragment" => end_frag = value.parse().ok(),
                    "Version" => {}
                    _ => break,
                }
                // 头部终止于下一行的起始（跳过本行行尾的 \r\n）
                header_end = lines.get(idx + 1).map_or(bytes.len(), |&(ns, _)| ns);
            }
            if let (Some(s), Some(e)) = (start_html, end_html) {
                if s <= e && e <= bytes.len() {
                    return (s, e);
                }
            }
            if let (Some(s), Some(e)) = (start_frag, end_frag) {
                if s <= e && e <= bytes.len() {
                    return (s, e);
                }
            }
            (header_end.min(bytes.len()), bytes.len())
        }

        /// 按行切分（处理 \r\n 与 \n），返回每行的字节区间
        fn split_lines(bytes: &[u8]) -> Vec<(usize, usize)> {
            let mut lines = Vec::new();
            let mut start = 0;
            for (i, &b) in bytes.iter().enumerate() {
                if b == b'\n' {
                    let mut end = i;
                    if end > start && bytes[end - 1] == b'\r' {
                        end -= 1;
                    }
                    lines.push((start, end));
                    start = i + 1;
                }
            }
            if start < bytes.len() {
                lines.push((start, bytes.len()));
            }
            lines
        }
    }
}

#[cfg(test)]
mod tests {
    // CF_HTML 解析单测：完整头 / 仅 fragment 头 / 无头裸 HTML
    // 偏移量按真实布局计算：固定宽度十进制（10 位），\r\n 行尾
    #[cfg(windows)]
    use crate::platform::snapshot::cf_html_range;

    #[cfg(windows)]
    use crate::platform::snapshot;

    /// 原始 Win32 直写 CF_UNICODETEXT + HTML Format（与验收脚本同路径），
    /// 验证 snapshot::read 判定为 RichText 而非 Text。
    #[cfg(windows)]
    #[test]
    fn snapshot_classifies_text_plus_html_as_rich() {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };

        unsafe {
            assert!(OpenClipboard(None).is_ok(), "打开剪贴板失败");
            let result = (|| {
                let _ = EmptyClipboard();
                let text = "SnapRichText 单测内容\0";
                let bytes: Vec<u16> = text.encode_utf16().collect();
                let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len() * 2).unwrap();
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    GlobalLock(h) as *mut u16,
                    bytes.len(),
                );
                let _ = GlobalUnlock(h);
                if SetClipboardData(13, Some(HANDLE(h.0))).is_err() {
                    return false;
                }
                let html = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000180\r\nStartFragment:0000000138\r\nEndFragment:0000000160\r\n<html><body>\r\n<!--StartFragment--><b>SnapRichText</b> 单测内容<!--EndFragment-->\r\n</body></html>";
                // CF_HTML 载荷按 UTF-8 字节写（真实协议如此）
                let hb = html.as_bytes().to_vec();
                let hh = GlobalAlloc(GMEM_MOVEABLE, hb.len()).unwrap();
                std::ptr::copy_nonoverlapping(hb.as_ptr(), GlobalLock(hh) as *mut u8, hb.len());
                let _ = GlobalUnlock(hh);
                let fmt = snapshot::register_format("HTML Format");
                if SetClipboardData(fmt, Some(HANDLE(hh.0))).is_err() {
                    return false;
                }
                true
            })();
            let _ = CloseClipboard();
            assert!(result, "直写失败");
        }

        let html_fmt = snapshot::register_format("HTML Format");
        let png_fmt = snapshot::register_format("PNG");
        let kind = snapshot::read(html_fmt, png_fmt).expect("快照读取失败（剪贴板持续被占用）");
        match kind {
            snapshot::Kind::RichText { text, html } => {
                assert!(text.contains("SnapRichText"), "text = {text}");
                assert!(html.contains("SnapRichText"), "html = {html}");
            }
            other => panic!("期望 RichText，实际 {other:?}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn cf_html_full_header_offsets() {
        // 头 105 字节：13 + 22 + 20 + 26 + 24；body 31 字节
        let header = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000136\r\nStartFragment:0000000117\r\nEndFragment:0000000122\r\n";
        let body = "<html><body>Hello</body></html>";
        let data = format!("{header}{body}");
        let (s, e) = cf_html_range(data.as_bytes());
        assert_eq!(&data.as_bytes()[s..e], b"<html><body>Hello</body></html>");
    }

    #[cfg(windows)]
    #[test]
    fn cf_html_fragment_only_offsets() {
        // 头 63 字节：13 + 26 + 24；"<html><body>" 12 字节后是 ABCD
        let header = "Version:0.9\r\nStartFragment:0000000075\r\nEndFragment:0000000079\r\n";
        let body = "<html><body>ABCD</body></html>";
        let data = format!("{header}{body}");
        let (s, e) = cf_html_range(data.as_bytes());
        assert_eq!(&data.as_bytes()[s..e], b"ABCD");
    }

    #[cfg(windows)]
    #[test]
    fn cf_html_no_offsets_strips_header() {
        let data = "Version:0.9\r\n<html><b>Hi</b></html>";
        let (s, e) = cf_html_range(data.as_bytes());
        assert_eq!(&data.as_bytes()[s..e], b"<html><b>Hi</b></html>");
    }
}
