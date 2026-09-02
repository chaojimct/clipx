//! WPF 版对齐：弹窗不取焦点，键盘输入经 WH_KEYBOARD_LL 低级钩子拦截。
//! M0 仅拦截 Esc 关闭；M1 键盘导航在此基础上扩展。

#[cfg(windows)]
mod platform {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::Mutex;

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
        WH_KEYBOARD_LL,
    };

    use slint::ComponentHandle;

    use crate::PopupWindow;

    const WM_KEYDOWN: usize = 0x0100;

    static HOOK: AtomicIsize = AtomicIsize::new(0);
    static WEAK: Mutex<Option<slint::Weak<PopupWindow>>> = Mutex::new(None);

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == 0 && wparam.0 == WM_KEYDOWN {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if kb.vkCode == VK_ESCAPE.0 as u32 {
                let weak = WEAK.lock().unwrap().clone();
                if let Some(weak) = weak {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let _ = ui.window().hide();
                        }
                        uninstall();
                    });
                }
                // 吞掉 Esc，避免同时触发前台应用的取消行为
                return LRESULT(1);
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

    /// 必须在事件循环线程调用（WH_KEYBOARD_LL 依赖安装线程的消息循环）。
    pub fn install(weak: slint::Weak<PopupWindow>) -> bool {
        if HOOK.load(Ordering::SeqCst) != 0 {
            return true;
        }
        unsafe {
            let Ok(handle) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) else {
                return false;
            };
            *WEAK.lock().unwrap() = Some(weak);
            HOOK.store(handle.0 as isize, Ordering::SeqCst);
        }
        true
    }

    pub fn uninstall() {
        let hhk = HOOK.swap(0, Ordering::SeqCst);
        if hhk != 0 {
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(hhk as *mut _));
            }
        }
        *WEAK.lock().unwrap() = None;
    }
}

#[cfg(windows)]
pub use platform::{install, uninstall};

#[cfg(not(windows))]
mod fallback {
    use crate::PopupWindow;
    pub fn install(_weak: slint::Weak<PopupWindow>) -> bool {
        true
    }
    pub fn uninstall() {}
}

#[cfg(not(windows))]
pub use fallback::{install, uninstall};
