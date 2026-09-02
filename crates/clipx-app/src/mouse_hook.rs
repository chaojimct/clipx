//! WPF 版对齐：WH_MOUSE_LL 钩子实现“弹窗可见时点击外部关闭”。
//! 弹窗为 WS_EX_NOACTIVATE，点击自身不改变前台窗口，
//! 因此外部点击必须由鼠标钩子捕捉（WPF 版同为 MouseHook 方案）。

use std::sync::atomic::AtomicIsize;

/// 弹窗 HWND（isize），显示时由逻辑层写入。预览面板与弹窗同窗口，
/// 点击预览区域天然属于"内部"，无需单独登记。
pub static POPUP_HWND: AtomicIsize = AtomicIsize::new(0);

type HideSender = std::sync::mpsc::Sender<()>;
static SENDER: std::sync::Mutex<Option<HideSender>> = std::sync::Mutex::new(None);

pub fn hide_channel() -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    *SENDER.lock().unwrap() = Some(tx);
    rx
}

#[cfg(windows)]
mod platform {
    use super::{POPUP_HWND, SENDER};

    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetCursorPos, GetWindowRect, SetWindowsHookExW, HHOOK, WH_MOUSE_LL,
    };

    const WM_LBUTTONDOWN: usize = 0x0201;
    const WM_RBUTTONDOWN: usize = 0x0204;

    static HOOK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

    fn cursor_inside(hwnd: HWND) -> bool {
        unsafe {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_err() {
                return true; // 拿不到矩形时保守视为内部，避免误关
            }
            let mut pt = POINT::default();
            if GetCursorPos(&mut pt).is_err() {
                return true;
            }
            pt.x >= rect.left && pt.x <= rect.right && pt.y >= rect.top && pt.y <= rect.bottom
        }
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == 0 && (wparam.0 == WM_LBUTTONDOWN || wparam.0 == WM_RBUTTONDOWN) {
            let popup = POPUP_HWND.load(Ordering::SeqCst);
            if popup != 0 && !cursor_inside(HWND(popup as *mut _)) {
                if let Some(tx) = SENDER.lock().unwrap().as_ref() {
                    let _ = tx.send(());
                }
            }
        }
        let hhk = HOOK.load(Ordering::SeqCst);
        let handle = if hhk == 0 { None } else { Some(HHOOK(hhk as *mut _)) };
        CallNextHookEx(handle, code, wparam, lparam)
    }

    pub fn install() -> bool {
        if HOOK.load(Ordering::SeqCst) != 0 {
            return true;
        }
        unsafe {
            let Ok(handle) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) else {
                return false;
            };
            HOOK.store(handle.0 as isize, Ordering::SeqCst);
        }
        true
    }
}

#[cfg(windows)]
pub use platform::install;

#[cfg(not(windows))]
mod fallback {
    pub fn install() -> bool {
        true
    }
}

#[cfg(not(windows))]
pub use fallback::install;
