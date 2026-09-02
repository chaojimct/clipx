use anyhow::Result;
use clipboard_rs::{common::RustImage, Clipboard, ClipboardContext};

/// 粘贴：写回剪贴板（调用方负责先 arm ClipboardGate），隐藏弹窗后模拟 Ctrl+V 到前台应用。
/// clipboard-rs 的 set_text/set_html 均不清剪贴板：先 clear 再写，
/// 避免上一次复制的旧格式残留（如旧图片 DIB 与新文本并存）。
pub fn write_text(ctx: &ClipboardContext, text: &str) -> Result<()> {
    ctx.clear()
        .map_err(|e| anyhow::anyhow!("清空剪贴板失败: {e}"))?;
    ctx.set_text(text.to_string())
        .map_err(|e| anyhow::anyhow!("写回剪贴板失败: {e}"))
}

/// 图片粘贴：PNG bytes → 位图写入剪贴板（clipboard-rs 内部完成 PNG→DIB 转换，
/// 对应 WPF 版原生 DIB 优先路径；set_image 自带 clear）。
pub fn write_image(ctx: &ClipboardContext, png: &[u8]) -> Result<()> {
    let img = clipboard_rs::common::RustImageData::from_bytes(png)
        .map_err(|e| anyhow::anyhow!("图片解码失败: {e}"))?;
    ctx.set_image(img)
        .map_err(|e| anyhow::anyhow!("写回图片剪贴板失败: {e}"))
}

/// 文件列表粘贴：CF_HDROP 写回（对应 WPF SetFileDropList；set_files 自带 clear）。
pub fn write_files(ctx: &ClipboardContext, paths: &[String]) -> Result<()> {
    ctx.set_files(paths.to_vec())
        .map_err(|e| anyhow::anyhow!("写回文件列表剪贴板失败: {e}"))
}

/// 富文本粘贴：清空后写 CF_UNICODETEXT 投影 + HTML Format，
/// 纯文本目标拿投影、富文本目标还原格式（Word/浏览器）。
pub fn write_rich_text(ctx: &ClipboardContext, text: &str, html: &str) -> Result<()> {
    ctx.clear()
        .map_err(|e| anyhow::anyhow!("清空剪贴板失败: {e}"))?;
    ctx.set_text(text.to_string())
        .map_err(|e| anyhow::anyhow!("写回文本投影失败: {e}"))?;
    ctx.set_html(html.to_string())
        .map_err(|e| anyhow::anyhow!("写回 HTML Format 失败: {e}"))
}

#[cfg(windows)]
pub fn send_ctrl_v() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
        VK_CONTROL, VK_V,
    };

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        let ki = KEYBDINPUT {
            wVk: vk,
            dwFlags: if up {
                KEYEVENTF_KEYUP
            } else {
                Default::default()
            },
            ..Default::default()
        };
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki },
        }
    }

    let inputs = [
        key(VK_CONTROL, false),
        key(VK_V, false),
        key(VK_V, true),
        key(VK_CONTROL, true),
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(not(windows))]
pub fn send_ctrl_v() {}
