//! WPF 版对齐：WH_MOUSE_LL 钩子实现“弹窗可见时点击外部关闭”。
//! 弹窗为 WS_EX_NOACTIVATE，点击自身不改变前台窗口，
//! 因此外部点击必须由鼠标钩子捕捉（WPF 版同为 MouseHook 方案）。

use std::sync::atomic::AtomicIsize;

/// 弹窗 HWND（isize），显示时由逻辑层写入。预览面板与弹窗同窗口，
/// 点击预览区域天然属于"内部"，无需单独登记。
pub static POPUP_HWND: AtomicIsize = AtomicIsize::new(0);

/// FileJump Picker HWND（isize）：点击落在任一窗口内都视为内部，不关闭。
pub static FJ_HWND: AtomicIsize = AtomicIsize::new(0);

type HideSender = std::sync::mpsc::Sender<()>;
static SENDER: std::sync::Mutex<Option<HideSender>> = std::sync::Mutex::new(None);

pub fn hide_channel() -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    *SENDER.lock().unwrap() = Some(tx);
    rx
}

#[cfg(windows)]
mod platform {
    use super::{FJ_HWND, POPUP_HWND, SENDER};

    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetCursorPos, GetMessageW, GetWindowRect, PostThreadMessageW,
        SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, MSG, WH_MOUSE_LL, WM_QUIT,
    };

    const WM_LBUTTONDOWN: usize = 0x0201;
    const WM_RBUTTONDOWN: usize = 0x0204;

    static HOOK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

    /// 钩子**专用线程** id（同 keyboard_hook::HOOK_TID：LL 钩子回调超时会被
    /// 系统静默摘钩，不能装在 Slint UI 线程；2026-09-22 随键盘钩子一并搬迁）。
    static HOOK_TID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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
            // 存活心跳：前 3 次点击全记，之后每 50 次记一次。
            {
                static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                if n < 3 || n % 50 == 0 {
                    crate::win_popup::append_debug_log(
                        "hotkey_debug.log",
                        &format!("mouse hook alive #{n} wp=0x{:X}", wparam.0),
                    );
                }
            }
            let popup = POPUP_HWND.load(Ordering::SeqCst);
            let fj = FJ_HWND.load(Ordering::SeqCst);
            if popup == 0 && fj == 0 {
                // 无弹窗可见：FileJump 自动跳转的"首次左键兜底"由前台轮询覆盖，此处不处理
            } else {
                let inside_popup = popup != 0 && cursor_inside(HWND(popup as *mut _));
                let inside_fj = fj != 0 && cursor_inside(HWND(fj as *mut _));
                if fj != 0 {
                    crate::keyboard_hook::fj_note_click(inside_fj);
                }
                if !inside_popup && !inside_fj && !crate::win_popup::is_resizing() {
                    if let Some(tx) = SENDER.lock().unwrap().as_ref() {
                        let _ = tx.send(());
                    }
                }
            }
        }
        let hhk = HOOK.load(Ordering::SeqCst);
        let handle = if hhk == 0 {
            None
        } else {
            Some(HHOOK(hhk as *mut _))
        };
        CallNextHookEx(handle, code, wparam, lparam)
    }

    /// 在**专用线程**安装 LL 鼠标钩子并跑消息循环（同 keyboard_hook::install，
    /// 同步等待安装结果；避免 Slint UI 线程卡顿导致系统静默摘钩）。
    pub fn install() -> bool {
        if HOOK.load(Ordering::SeqCst) != 0 {
            return true;
        }
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let spawned = std::thread::Builder::new()
            .name("clipx-mouse-hook".into())
            .spawn(move || unsafe {
                HOOK_TID.store(GetCurrentThreadId(), Ordering::SeqCst);
                match SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) {
                    Ok(handle) => {
                        HOOK.store(handle.0 as isize, Ordering::SeqCst);
                        crate::win_popup::append_debug_log(
                            "hotkey_debug.log",
                            &format!("mouse hook installed h=0x{:X}", handle.0 as isize),
                        );
                        let _ = tx.send(true);
                        let my_hook = handle;
                        let mut msg = MSG::default();
                        loop {
                            let r = GetMessageW(&mut msg, None, 0, 0);
                            if r.0 <= 0 {
                                break;
                            }
                        }
                        let _ = UnhookWindowsHookEx(my_hook);
                    }
                    Err(e) => {
                        crate::win_popup::append_debug_log(
                            "hotkey_debug.log",
                            &format!("mouse hook install FAILED: {e}"),
                        );
                        let _ = tx.send(false);
                    }
                }
                HOOK_TID.store(0, Ordering::SeqCst);
            });
        match spawned {
            Err(_) => false,
            Ok(_) => rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(false),
        }
    }

    pub fn uninstall() {
        use windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx;
        let hhk = HOOK.swap(0, Ordering::SeqCst);
        if hhk != 0 {
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(hhk as *mut _));
            }
        }
        let tid = HOOK_TID.load(Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

#[cfg(windows)]
pub use platform::{install, uninstall};

/// 同 keyboard_hook::reinstall（钩子已搬专用线程，本函数保留为看门狗兜底）。
#[cfg(windows)]
pub fn reinstall() {
    uninstall();
    let _ = install();
}

#[cfg(not(windows))]
pub fn reinstall() {}

#[cfg(not(windows))]
mod fallback {
    pub fn install() -> bool {
        true
    }
}

#[cfg(not(windows))]
pub use fallback::install;
