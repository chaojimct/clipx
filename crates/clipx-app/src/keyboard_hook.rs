//! WPF 版对齐：弹窗不取焦点，键盘输入经 WH_KEYBOARD_LL 低级钩子拦截。
//! 弹窗可见期间吞掉普通按键（防止漏给前台应用），翻译后转发给逻辑线程；
//! 修饰键组合（Ctrl/Alt/Win）放行，保证热键与系统快捷键可用。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyEvt {
    Char(char),
    Digit(u8),
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    Delete,
    Home,
    End,
    PgUp,
    PgDn,
    /// Space：切换选中条目预览（对齐 WPF 版），不作为搜索字符
    Space,
    /// Ctrl+P：切换选中条目置顶/收藏
    PinToggle,
    /// Menu 键（VK_APPS）：打开选中条目的上下文菜单
    Menu,
}

static VISIBLE: AtomicBool = AtomicBool::new(false);

pub fn set_visible(v: bool) {
    VISIBLE.store(v, Ordering::SeqCst);
}

pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::SeqCst)
}

type EvtSender = std::sync::mpsc::Sender<KeyEvt>;
static SENDER: Mutex<Option<EvtSender>> = Mutex::new(None);

pub fn evt_channel() -> std::sync::mpsc::Receiver<KeyEvt> {
    let (tx, rx) = std::sync::mpsc::channel();
    *SENDER.lock().unwrap() = Some(tx);
    rx
}

#[cfg(windows)]
mod platform {
    use super::{is_visible, KeyEvt, SENDER};

    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE,
        VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_RWIN,
        VK_SHIFT, VK_UP,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
        WH_KEYBOARD_LL,
    };

    const WM_KEYDOWN: usize = 0x0100;
    const WM_SYSKEYDOWN: usize = 0x0104;

    static HOOK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == 0 && (wparam.0 == WM_KEYDOWN || wparam.0 == WM_SYSKEYDOWN) && is_visible() {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if let Some(evt) = translate(kb.vkCode) {
                if let Some(tx) = SENDER.lock().unwrap().as_ref() {
                    let _ = tx.send(evt);
                }
                // 吞掉，避免漏给前台应用
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

    fn shift_down() -> bool {
        unsafe { (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16) & 0x8000 != 0 }
    }

    fn ctrl_or_alt_or_win_down() -> bool {
        unsafe {
            ((GetAsyncKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0)
                || ((GetAsyncKeyState(VK_MENU.0 as i32) as u16) & 0x8000 != 0)
                || ((GetAsyncKeyState(VK_LWIN.0 as i32) as u16) & 0x8000 != 0)
                || ((GetAsyncKeyState(VK_RWIN.0 as i32) as u16) & 0x8000 != 0)
        }
    }

    /// 返回 None = 不拦截（修饰键本身 / 修饰键组合 / 不支持的键）。
    fn translate(vk: u32) -> Option<KeyEvt> {
        // Ctrl+P：切换选中条目置顶（仅 Ctrl，无 Alt/Win，避免吞掉系统组合）
        if ctrl_only_down() && vk == 0x50 {
            return Some(KeyEvt::PinToggle);
        }
        if ctrl_or_alt_or_win_down() {
            return None;
        }
        let shifted = shift_down();
        let vk = VIRTUAL_KEY(vk as u16);
        match vk {
            VK_BACK => Some(KeyEvt::Backspace),
            VK_RETURN => Some(KeyEvt::Enter),
            VK_ESCAPE => Some(KeyEvt::Esc),
            VK_DELETE => Some(KeyEvt::Delete),
            VK_HOME => Some(KeyEvt::Home),
            VK_END => Some(KeyEvt::End),
            VK_PRIOR => Some(KeyEvt::PgUp),
            VK_NEXT => Some(KeyEvt::PgDn),
            VK_UP | VK_LEFT => Some(KeyEvt::Up),
            VK_DOWN | VK_RIGHT => Some(KeyEvt::Down),
            _ if vk.0 == 0x20 => Some(KeyEvt::Space),
            // Menu 键（VK_APPS）：对选中条目打开上下文菜单
            _ if vk.0 == 0x5D => Some(KeyEvt::Menu),
            _ => char_from_vk(vk.0, shifted),
        }
    }

    fn ctrl_only_down() -> bool {
        unsafe {
            ((GetAsyncKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0)
                && ((GetAsyncKeyState(VK_MENU.0 as i32) as u16) & 0x8000 == 0)
                && ((GetAsyncKeyState(VK_LWIN.0 as i32) as u16) & 0x8000 == 0)
                && ((GetAsyncKeyState(VK_RWIN.0 as i32) as u16) & 0x8000 == 0)
        }
    }

    fn char_from_vk(vk: u16, shifted: bool) -> Option<KeyEvt> {
        let c = match vk {
            0x30..=0x39 => {
                // 数字：搜索中作为字符，空搜索下由逻辑层决定是否快贴
                return Some(if shifted {
                    KeyEvt::Char("!@#$%^&*()".as_bytes()[(vk - 0x30) as usize] as char)
                } else {
                    KeyEvt::Digit((vk - 0x30) as u8)
                });
            }
            // 小键盘数字：NumLock 开启时同样作为数字输入
            0x60..=0x69 => return Some(KeyEvt::Digit((vk - 0x60) as u8)),
            0x41..=0x5A => {
                let c = (b'a' + (vk - 0x41) as u8) as char;
                if shifted {
                    c.to_ascii_uppercase()
                } else {
                    c
                }
            }
            0xBA => {
                if shifted {
                    ':'
                } else {
                    ';'
                }
            }
            0xBB => {
                if shifted {
                    '+'
                } else {
                    '='
                }
            }
            0xBC => {
                if shifted {
                    '<'
                } else {
                    ','
                }
            }
            0xBD => {
                if shifted {
                    '_'
                } else {
                    '-'
                }
            }
            0xBE => {
                if shifted {
                    '>'
                } else {
                    '.'
                }
            }
            0xBF => {
                if shifted {
                    '?'
                } else {
                    '/'
                }
            }
            0xC0 => {
                if shifted {
                    '~'
                } else {
                    '`'
                }
            }
            0xDB => {
                if shifted {
                    '{'
                } else {
                    '['
                }
            }
            0xDC => {
                if shifted {
                    '|'
                } else {
                    '\\'
                }
            }
            0xDD => {
                if shifted {
                    '}'
                } else {
                    ']'
                }
            }
            0xDE => {
                if shifted {
                    '"'
                } else {
                    '\''
                }
            }
            _ => return None, // F 键 / Tab / 修饰键等：放行
        };
        Some(KeyEvt::Char(c))
    }

    /// 必须在事件循环线程调用（WH_KEYBOARD_LL 依赖安装线程的消息循环）。
    pub fn install() -> bool {
        if HOOK.load(Ordering::SeqCst) != 0 {
            return true;
        }
        unsafe {
            let Ok(handle) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) else {
                return false;
            };
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
    }
}

#[cfg(windows)]
pub use platform::{install, uninstall};

#[cfg(not(windows))]
mod fallback {
    pub fn install() -> bool {
        true
    }
    pub fn uninstall() {}
}

#[cfg(not(windows))]
pub use fallback::{install, uninstall};
