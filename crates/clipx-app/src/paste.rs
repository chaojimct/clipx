use anyhow::Result;
use clipboard_rs::{common::RustImage, Clipboard, ClipboardContent, ClipboardContext};

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
    // set_text / set_html 各自会 EmptyClipboard，后写的 HTML 会把纯文本冲掉，
    // 记事本等只认 CF_UNICODETEXT 的目标就会贴出空串。必须一次写入两种格式。
    let mut contents = vec![ClipboardContent::Text(text.to_string())];
    if !html.trim().is_empty() {
        contents.push(ClipboardContent::Html(html.to_string()));
    }
    match ctx.set(contents) {
        Ok(()) => Ok(()),
        Err(e) => ctx
            .set_text(text.to_string())
            .map_err(|e2| anyhow::anyhow!("写回富文本失败: {e}; 纯文本回退也失败: {e2}")),
    }
}

#[cfg(windows)]
pub fn send_paste(mode: &str) {
    if mode == "ShiftInsert" {
        send_shift_insert();
    } else {
        send_ctrl_v();
    }
}

#[cfg(not(windows))]
pub fn send_paste(_mode: &str) {}

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

#[cfg(windows)]
fn send_shift_insert() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
        VK_INSERT, VK_SHIFT,
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
        key(VK_SHIFT, false),
        key(VK_INSERT, false),
        key(VK_INSERT, true),
        key(VK_SHIFT, true),
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(not(windows))]
pub fn send_ctrl_v() {}

/// 目标像控制台时用 Shift+Insert（WPF PasteTargetHeuristics）。
pub fn paste_mode_for_target(hwnd: isize, configured: &str) -> &str {
    if is_console_hwnd(hwnd) {
        "ShiftInsert"
    } else {
        configured
    }
}

fn is_console_hwnd(hwnd: isize) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindow, GW_OWNER};
        if hwnd == 0 {
            return false;
        }
        unsafe {
            let h = HWND(hwnd as *mut _);
            let mut buf = [0u16; 256];
            let n = GetClassNameW(h, &mut buf);
            let class = String::from_utf16_lossy(&buf[..n as usize]);
            if [
                "ConsoleWindowClass",
                "CASCADIA_HOSTING_WINDOW_CLASS",
                "PseudoConsoleWindow",
            ]
            .iter()
            .any(|c| class.eq_ignore_ascii_case(c))
            {
                return true;
            }
            if let Ok(owner) = GetWindow(h, GW_OWNER) {
                if !owner.0.is_null() {
                    let n = GetClassNameW(owner, &mut buf);
                    let oc = String::from_utf16_lossy(&buf[..n as usize]);
                    if oc.eq_ignore_ascii_case("CASCADIA_HOSTING_WINDOW_CLASS") {
                        return true;
                    }
                }
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        false
    }
}

/// 作为文件粘贴：落临时文件后写 CF_HDROP。
pub fn write_temp_files(ctx: &ClipboardContext, files: &[(String, Vec<u8>)]) -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("clipx-paste");
    std::fs::create_dir_all(&dir)?;
    let mut paths = Vec::new();
    for (name, bytes) in files {
        let p = dir.join(name);
        std::fs::write(&p, bytes)?;
        paths.push(p.to_string_lossy().into_owned());
    }
    write_files(ctx, &paths)
}
