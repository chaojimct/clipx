#[derive(Debug, Clone)]
pub enum ClipEvent {
    Text(String),
    Image {
        blob: Vec<u8>,
        width: u32,
        height: u32,
        mime: String,
    },
    Files(Vec<String>),
    /// 富文本：同时携带纯文本与 HTML（粘贴时优先还原 HTML）
    RichText {
        text: String,
        html: String,
    },
}
