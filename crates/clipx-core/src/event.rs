#[derive(Debug, Clone)]
pub enum ClipEvent {
    Text(String),
    Image {
        blob: Vec<u8>,
        width: u32,
        height: u32,
        mime: String,
        /// 捕获时从已解码位图压出的 64px 列表缩略图（PNG）；空则入库侧回退解码。
        thumb: Vec<u8>,
        thumb_w: u32,
        thumb_h: u32,
        /// 捕获时从已解码位图压出的 1280 JPEG 预览图；空则 rendition worker 补做。
        rendition_jpeg: Vec<u8>,
    },
    Files(Vec<String>),
    /// 富文本：同时携带纯文本与 HTML（粘贴时优先还原 HTML）
    RichText {
        text: String,
        html: String,
    },
}
