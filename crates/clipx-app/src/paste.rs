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

/// 粘贴模拟前要抬起的修饰键 `(vk, extended)`。
/// Ctrl+1 快贴时 Ctrl 仍按着：不抬起，Shift+Insert 会变成 Ctrl+Shift+Insert，终端不认。
pub fn paste_modifier_releases(also_shift: bool, held: impl Fn(u16) -> bool) -> Vec<(u16, bool)> {
    const VK_LSHIFT: u16 = 0xA0;
    const VK_RSHIFT: u16 = 0xA1;
    const VK_CONTROL: u16 = 0x11;
    const VK_LCONTROL: u16 = 0xA2;
    const VK_RCONTROL: u16 = 0xA3;
    const VK_MENU: u16 = 0x12;
    const VK_LWIN: u16 = 0x5B;
    const VK_RWIN: u16 = 0x5C;
    let mut out = Vec::new();
    if also_shift {
        if held(VK_LSHIFT) {
            out.push((VK_LSHIFT, false));
        }
        if held(VK_RSHIFT) {
            out.push((VK_RSHIFT, false));
        }
    }
    if held(VK_CONTROL) || held(VK_LCONTROL) || held(VK_RCONTROL) {
        out.push((VK_CONTROL, false));
        if held(VK_LCONTROL) {
            out.push((VK_LCONTROL, false));
        }
        if held(VK_RCONTROL) {
            out.push((VK_RCONTROL, false));
        }
    }
    if held(VK_MENU) {
        out.push((VK_MENU, false));
    }
    if held(VK_LWIN) {
        out.push((VK_LWIN, true));
    }
    if held(VK_RWIN) {
        out.push((VK_RWIN, true));
    }
    out
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
fn vk_physically_down(vk: u16) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    unsafe { (GetAsyncKeyState(vk as i32) as u16) & 0x8000 != 0 }
}

#[cfg(windows)]
fn keyup_input(vk: u16, extended: bool) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        VIRTUAL_KEY,
    };
    let mut flags = KEYEVENTF_KEYUP;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

/// 先抬起仍按着的修饰键，再发粘贴组合（对齐 WPF SendCtrlVPaste / SendShiftInsertPaste）。
#[cfg(windows)]
fn release_held_modifiers(also_shift: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::SendInput;
    let keys = paste_modifier_releases(also_shift, vk_physically_down);
    if keys.is_empty() {
        return;
    }
    let inputs: Vec<_> = keys
        .iter()
        .map(|(vk, ext)| keyup_input(*vk, *ext))
        .collect();
    unsafe {
        SendInput(&inputs, std::mem::size_of::<windows::Win32::UI::Input::KeyboardAndMouse::INPUT>() as i32);
    }
    std::thread::sleep(std::time::Duration::from_millis(1));
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

    release_held_modifiers(true);
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
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_INSERT, VK_SHIFT,
    };

    fn key(vk: VIRTUAL_KEY, up: bool, extended: bool) -> INPUT {
        let mut flags = if up {
            KEYEVENTF_KEYUP
        } else {
            Default::default()
        };
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        let ki = KEYBDINPUT {
            wVk: vk,
            dwFlags: flags,
            ..Default::default()
        };
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki },
        }
    }

    // Insert 是扩展键；不带 EXTENDEDKEY 时终端往往收不到 Shift+Insert 粘贴。
    release_held_modifiers(false);
    let inputs = [
        key(VK_SHIFT, false, false),
        key(VK_INSERT, false, true),
        key(VK_INSERT, true, true),
        key(VK_SHIFT, true, false),
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 段间等待目标消费剪贴板（对齐 WPF `WaitForTargetClipboardConsumeAsync`）。
#[cfg(windows)]
pub fn wait_clipboard_consumed(own_hwnd: isize, after_image: bool) {
    use windows::Win32::System::DataExchange::{
        GetClipboardSequenceNumber, GetOpenClipboardWindow,
    };
    let max_ms = if after_image { 600u128 } else { 350 };
    let start_seq = unsafe { GetClipboardSequenceNumber() };
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_millis() < max_ms {
        std::thread::sleep(std::time::Duration::from_millis(12));
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq != start_seq {
            break;
        }
        let owner = unsafe {
            GetOpenClipboardWindow()
                .map(|h| h.0 as isize)
                .unwrap_or(0)
        };
        if owner != 0 && owner != own_hwnd {
            std::thread::sleep(std::time::Duration::from_millis(20));
            break;
        }
    }
}

#[cfg(not(windows))]
pub fn wait_clipboard_consumed(_own_hwnd: isize, _after_image: bool) {
    std::thread::sleep(std::time::Duration::from_millis(40));
}

#[cfg(not(windows))]
pub fn send_ctrl_v() {}

/// 终端粘贴：去掉 CR。Linux PTY 把 `\r` 显示成 `^M`；bash 里 Ctrl+V 是「下一键原样插入」，
/// 再按 Enter 就会打出 `^M`。系统 Shift+Insert / 终端自己的粘贴会拦快捷键，不会进 shell。
pub fn normalize_console_text(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn write_text_for_target(
    ctx: &ClipboardContext,
    text: &str,
    for_console: bool,
) -> Result<()> {
    if for_console {
        write_text(ctx, &normalize_console_text(text))
    } else {
        write_text(ctx, text)
    }
}

/// 窗口类是否终端宿主（conhost / Windows Terminal / mintty 等）。
pub fn is_terminal_class_name(class: &str) -> bool {
    let c = class.to_ascii_lowercase();
    matches!(
        c.as_str(),
        "consolewindowclass"
            | "cascadia_hosting_window_class"
            | "pseudoconsolewindow"
            | "mintty"
            | "putty"
            | "kitty"
            | "alacritty"
    ) || c.contains("wezterm")
        || c.contains("xshell")
        || c.contains("mobaxterm")
        || c.contains("tabby")
}

/// 进程名是否终端（对齐 WPF PasteTargetHeuristics，并补 SSH 客户端 / Cursor 集成终端）。
pub fn is_terminal_process_name(s: &str) -> bool {
    let file = s.rsplit(['\\', '/']).next().unwrap_or(s);
    let n = file.to_ascii_lowercase();
    let n = n.strip_suffix(".exe").unwrap_or(&n);
    matches!(
        n,
        "cmd"
            | "powershell"
            | "pwsh"
            | "windowsterminal"
            | "wt"
            | "openconsole"
            | "conhost"
            | "mintty"
            | "bash"
            | "wsl"
            | "wslhost"
            | "wezterm-gui"
            | "wezterm"
            | "putty"
            | "kitty"
            | "tabby"
            | "alacritty"
            | "xshell"
            | "xshell5"
            | "xshell6"
            | "xshell7"
            | "mobaxterm"
            | "cursor"
            | "code"
            | "code - insiders"
    )
}

/// 目标像控制台时用 Shift+Insert（WPF PasteTargetHeuristics）。
pub fn paste_mode_for_target(hwnd: isize, configured: &str) -> &str {
    if is_console_target(hwnd) {
        "ShiftInsert"
    } else {
        configured
    }
}

pub fn is_console_target(hwnd: isize) -> bool {
    is_console_hwnd(hwnd)
}

fn is_console_hwnd(hwnd: isize) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GetWindow, GA_ROOT, GW_OWNER,
        };
        if hwnd == 0 {
            return false;
        }
        unsafe {
            let h = HWND(hwnd as *mut _);
            if hwnd_looks_terminal(h) {
                return true;
            }
            if let Ok(owner) = GetWindow(h, GW_OWNER) {
                if !owner.0.is_null() && hwnd_looks_terminal(owner) {
                    return true;
                }
            }
            let root = GetAncestor(h, GA_ROOT);
            if !root.0.is_null() && root != h && hwnd_looks_terminal(root) {
                return true;
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

#[cfg(windows)]
fn hwnd_looks_terminal(h: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf = [0u16; 256];
    let class = unsafe {
        let n = GetClassNameW(h, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    };
    if is_terminal_class_name(&class) {
        return true;
    }
    is_terminal_process_name(&hwnd_exe_stem(h))
}

#[cfg(windows)]
fn hwnd_exe_stem(h: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
    }
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let Ok(proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let name = if QueryFullProcessImageNameW(
            proc,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok()
        {
            String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())])
        } else {
            String::new()
        };
        let _ = windows::Win32::Foundation::CloseHandle(proc);
        name
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_console_text_drops_cr() {
        assert_eq!(normalize_console_text("wget foo\r\n"), "wget foo\n");
        assert_eq!(normalize_console_text("wget foo\r"), "wget foo\n");
        assert_eq!(normalize_console_text("a\rb"), "a\nb");
        assert_eq!(normalize_console_text("plain"), "plain");
    }

    #[test]
    fn terminal_class_and_process() {
        assert!(is_terminal_class_name("CASCADIA_HOSTING_WINDOW_CLASS"));
        assert!(is_terminal_class_name("ConsoleWindowClass"));
        assert!(!is_terminal_class_name("Chrome_WidgetWin_1"));
        assert!(is_terminal_process_name("WindowsTerminal.exe"));
        assert!(is_terminal_process_name("Cursor"));
        assert!(is_terminal_process_name(r"C:\Program Files\cursor\Cursor.exe"));
        assert!(!is_terminal_process_name("WorkBuddy.exe"));
        assert!(!is_terminal_process_name("msedge"));
        assert_eq!(paste_mode_for_target(0, "CtrlV"), "CtrlV");
    }

    #[test]
    fn ctrl_digit_paste_releases_control() {
        let held = |vk| vk == 0x11;
        let rel = paste_modifier_releases(false, held);
        assert!(rel.iter().any(|(vk, _)| *vk == 0x11));
        assert!(!rel.iter().any(|(vk, _)| *vk == 0xA0));
    }

    #[test]
    fn ctrl_v_also_releases_shift() {
        let held = |vk| vk == 0x11 || vk == 0xA0;
        let rel = paste_modifier_releases(true, held);
        assert!(rel.iter().any(|(vk, _)| *vk == 0x11));
        assert!(rel.iter().any(|(vk, _)| *vk == 0xA0));
    }
}
