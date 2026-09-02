use anyhow::Result;
use clipboard_rs::{Clipboard, ClipboardContext};

/// 粘贴：写回剪贴板（调用方负责先 arm ClipboardGate），隐藏弹窗后模拟 Ctrl+V 到前台应用。
pub fn write_text(ctx: &ClipboardContext, text: &str) -> Result<()> {
    ctx.set_text(text.to_string())
        .map_err(|e| anyhow::anyhow!("写回剪贴板失败: {e}"))
}

#[cfg(windows)]
pub fn send_ctrl_v() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
        VK_CONTROL, VK_V,
    };

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        let mut ki = KEYBDINPUT::default();
        ki.wVk = vk;
        if up {
            ki.dwFlags = KEYEVENTF_KEYUP;
        }
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
