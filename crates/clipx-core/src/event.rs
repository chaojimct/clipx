#[derive(Debug, Clone)]
pub enum ClipEvent {
    Text(String),
    Image { blob: Vec<u8>, width: u32, height: u32, mime: String },
    Files(Vec<String>),
}
