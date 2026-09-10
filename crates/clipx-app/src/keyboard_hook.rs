//! WPF 版对齐：弹窗不取焦点，键盘输入经 WH_KEYBOARD_LL 低级钩子拦截。
//! 弹窗可见期间吞掉普通按键（防止漏给前台应用），翻译后转发给逻辑线程；
//! 修饰键组合（Ctrl/Alt/Win）放行，保证热键与系统快捷键可用。
//!
//! 快速查找（M4）：弹窗隐藏时，Explorer/桌面上下文中的普通字符键触发
//! Everything 快速查找会话；会话期间全面接管键盘（对齐 WPF
//! ExplorerQuickFindController 的钩子快路径，仅 Win32 调用，<1ms）。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
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
    /// 设置窗口热键录制：原始虚键码 + 按下瞬间的修饰键快照
    ///（Esc=0x1B 由逻辑层判取消；逻辑层不再现读 GetAsyncKeyState，避免快按快松读到空）
    RecordVk(u32, u32),
    /// 面板主键+数字（m+N，WPF DisplayIndex 快贴）
    QuickNum(u8),
    /// 面板主键+Tab（WPF 快捷短语过滤开关）
    QuickTab,
    /// 批量队列：目标应用 Ctrl+V / Shift+Insert 松键后推进队首
    BatchAdvance,
    /// Shift+↑ / Shift+↓：扩选
    ShiftUp,
    ShiftDown,
    /// ← / →：列表翻页（对齐 WPF ScrollPage，不是上下移动）
    Left,
    Right,
    /// Tab：循环类型筛选
    Tab,
    /// Ctrl+Enter：多选连贴（文本之间加换行）
    CtrlEnter,
    /// Shift+Enter：粘贴 OCR 文字
    ShiftEnter,
    /// 单击 Alt 松开：开右键菜单；批量模式下一次贴完队列
    AltTap,
    /// 钩子拦截 Win+V（并注入 Win KeyUp，避免闪开始菜单）
    WinV,
    /// 面板可见时命中呼出热键：隐藏（勿把主键打进搜索）
    Toggle,
    /// 面板可见时命中 FileJump 热键
    FileJumpHotkey,
    /// 面板可见时命中批量模式热键
    BatchHotkey,
    // ===== 快速查找会话事件（M4，仅 Explorer 上下文）=====
    /// 字符键触发新会话（hook 侧已置位会话；逻辑线程做文件夹解析）
    QfStart {
        frame: isize,
        desktop: bool,
        ch: char,
    },
    /// 会话结束（前台离开/焦点入编辑框/修饰键组合），按键放行
    QfEnd,
    QfChar(char),
    QfEnter,
    QfEsc,
    QfBackspace,
    QfUp,
    QfDown,
    /// ←→/PgUp/PgDn 翻页（方向 ±1）
    QfPage(i32),
    QfHome,
    QfEndKey,
    /// Ctrl+1..9 快选
    QfQuickSelect(u8),
}

impl KeyEvt {
    pub fn is_qf(&self) -> bool {
        matches!(
            self,
            Self::QfStart { .. }
                | Self::QfEnd
                | Self::QfChar(_)
                | Self::QfEnter
                | Self::QfEsc
                | Self::QfBackspace
                | Self::QfUp
                | Self::QfDown
                | Self::QfPage(_)
                | Self::QfHome
                | Self::QfEndKey
                | Self::QfQuickSelect(_)
        )
    }
}

static VISIBLE: AtomicBool = AtomicBool::new(false);

/// 热键录制槽（设置窗口用）：-1=关闭，否则为槽位 id；
/// 开启时所有按键被吞并以 RecordVk 上报，由逻辑层组修饰键。
static RECORDING: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
/// 批量队列非空且弹窗隐藏时，监听目标应用的粘贴键松键。
static BATCH_WATCH: AtomicBool = AtomicBool::new(false);

/// 弹窗内自定义翻页热键（WPF PanelPageScrollUp/Down，默认 Ctrl+- / Ctrl+=）。
static PAGE_UP_MOD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x0002);
static PAGE_UP_VK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xBD);
static PAGE_DN_MOD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x0002);
static PAGE_DN_VK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xBB);

/// 替换系统 Win+V：钩子拦截并注入 Win KeyUp。
static REPLACE_WIN_V: AtomicBool = AtomicBool::new(false);
static WIN_V_INTERCEPTED: AtomicBool = AtomicBool::new(false);
/// 弹窗钉住或点外不关时，Explorer 前台按键放行。
#[allow(dead_code)]
static EXPLORER_PASSTHROUGH: AtomicBool = AtomicBool::new(false);
/// FileJump Picker 可见：默认吞键进过滤；用户点击对话框输入框后放行。
static FJ_PICKER: AtomicBool = AtomicBool::new(false);
static FJ_DIALOG: AtomicIsize = AtomicIsize::new(0);
static FJ_TYPE_PASSTHROUGH: AtomicBool = AtomicBool::new(false);

/// 钩子自己记账的 Shift/Ctrl：不信 GetAsyncKeyState / Slint modifiers（热键呼出后会粘住）。
static SHIFT_HELD: AtomicBool = AtomicBool::new(false);
static CTRL_HELD: AtomicBool = AtomicBool::new(false);
/// 呼出时若修饰键仍按着，必须先松开再按下，鼠标多选才生效。
static SHIFT_CLICK_OK: AtomicBool = AtomicBool::new(true);
static CTRL_CLICK_OK: AtomicBool = AtomicBool::new(true);

pub fn arm_click_modifiers() {
    SHIFT_CLICK_OK.store(!SHIFT_HELD.load(Ordering::SeqCst), Ordering::SeqCst);
    CTRL_CLICK_OK.store(!CTRL_HELD.load(Ordering::SeqCst), Ordering::SeqCst);
}

pub fn click_shift() -> bool {
    SHIFT_HELD.load(Ordering::SeqCst) && SHIFT_CLICK_OK.load(Ordering::SeqCst)
}

pub fn click_ctrl() -> bool {
    CTRL_HELD.load(Ordering::SeqCst) && CTRL_CLICK_OK.load(Ordering::SeqCst)
}

fn note_modifier(vk: u32, down: bool) {
    match vk {
        0x10 | 0xA0 | 0xA1 => {
            SHIFT_HELD.store(down, Ordering::SeqCst);
            if !down {
                SHIFT_CLICK_OK.store(true, Ordering::SeqCst);
            }
        }
        0x11 | 0xA2 | 0xA3 => {
            CTRL_HELD.store(down, Ordering::SeqCst);
            if !down {
                CTRL_CLICK_OK.store(true, Ordering::SeqCst);
            }
        }
        _ => {}
    }
}

pub fn set_replace_win_v(v: bool) {
    REPLACE_WIN_V.store(v, Ordering::SeqCst);
    if !v {
        WIN_V_INTERCEPTED.store(false, Ordering::SeqCst);
    }
}

#[allow(dead_code)]
pub fn set_explorer_passthrough(v: bool) {
    EXPLORER_PASSTHROUGH.store(v, Ordering::SeqCst);
}

pub fn set_fj_picker(v: bool) {
    FJ_PICKER.store(v, Ordering::SeqCst);
    if v {
        FJ_TYPE_PASSTHROUGH.store(false, Ordering::SeqCst);
    }
}

pub fn set_fj_dialog(hwnd: isize) {
    FJ_DIALOG.store(hwnd, Ordering::SeqCst);
}

/// 鼠标按下：点 Picker 恢复过滤；点对话框文件名/编辑框则放行键入。
pub fn fj_note_click(inside_picker: bool) {
    if !FJ_PICKER.load(Ordering::SeqCst) {
        return;
    }
    if inside_picker {
        FJ_TYPE_PASSTHROUGH.store(false, Ordering::SeqCst);
        return;
    }
    #[cfg(windows)]
    {
        let d = FJ_DIALOG.load(Ordering::SeqCst);
        if d != 0 && crate::explorer_shell::cursor_hits_dialog_edit(d) {
            FJ_TYPE_PASSTHROUGH.store(true, Ordering::SeqCst);
        }
    }
}

pub fn set_batch_watch(v: bool) {
    BATCH_WATCH.store(v, Ordering::SeqCst);
}

pub fn set_page_hotkeys(up: crate::settings::Hotkey, down: crate::settings::Hotkey) {
    PAGE_UP_MOD.store(up.modifiers, Ordering::SeqCst);
    PAGE_UP_VK.store(up.key, Ordering::SeqCst);
    PAGE_DN_MOD.store(down.modifiers, Ordering::SeqCst);
    PAGE_DN_VK.store(down.key, Ordering::SeqCst);
}

static CLIP_MOD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x0002);
static CLIP_VK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xC0);
static BATCH_MOD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x0001);
static BATCH_VK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xBF);
static FJ_MOD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x0002);
static FJ_VK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x47);

pub fn set_app_hotkeys(
    clip: crate::settings::Hotkey,
    batch: crate::settings::Hotkey,
    fj: crate::settings::Hotkey,
) {
    CLIP_MOD.store(clip.modifiers, Ordering::SeqCst);
    CLIP_VK.store(clip.key, Ordering::SeqCst);
    BATCH_MOD.store(batch.modifiers, Ordering::SeqCst);
    BATCH_VK.store(batch.key, Ordering::SeqCst);
    FJ_MOD.store(fj.modifiers, Ordering::SeqCst);
    FJ_VK.store(fj.key, Ordering::SeqCst);
}

/// 按键穿透快照（设置保存热更新；钩子只读）。
#[derive(Clone, Default)]
struct PassthroughCfg {
    enabled: bool,
    mask: u32,
    keep_panel: bool,
    rules: Vec<crate::settings::PassthroughRule>,
}

static PT_CFG: Mutex<PassthroughCfg> = Mutex::new(PassthroughCfg {
    enabled: false,
    mask: 0,
    keep_panel: true,
    rules: Vec::new(),
});
static PT_LATCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn set_passthrough(
    enabled: bool,
    mask: u32,
    keep_panel: bool,
    rules: &[crate::settings::PassthroughRule],
) {
    *PT_CFG.lock().unwrap() = PassthroughCfg {
        enabled,
        mask,
        keep_panel,
        rules: rules.to_vec(),
    };
    if !enabled {
        PT_LATCH.store(0, Ordering::SeqCst);
    }
}

pub fn set_recording(slot: i32) {
    RECORDING.store(slot, Ordering::SeqCst);
}

pub fn recording_slot() -> i32 {
    RECORDING.load(Ordering::SeqCst)
}

pub fn set_visible(v: bool) {
    VISIBLE.store(v, Ordering::SeqCst);
    if !v {
        set_edit_mode(0);
    }
}

/// 0=关 1=编辑文本（Esc / Ctrl+Enter 仍拦截）2=短语（Esc / Enter 拦截）
static EDIT_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_edit_mode(mode: u8) {
    EDIT_MODE.store(mode, Ordering::SeqCst);
}

pub fn edit_mode() -> u8 {
    EDIT_MODE.load(Ordering::SeqCst)
}

pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::SeqCst)
}

// ===== 快速查找会话的钩线程快速状态（跨线程原子读写）=====
static QF_ENABLED: AtomicBool = AtomicBool::new(false);
static QF_ACTIVE: AtomicBool = AtomicBool::new(false);
static QF_FRAME: AtomicIsize = AtomicIsize::new(0);

/// 启动时由主线程按设置写入。
pub fn set_qf_enabled(v: bool) {
    QF_ENABLED.store(v, Ordering::SeqCst);
}

/// hook 侧检测到启动条件时置位（对齐 WPF _sessionActive）。
pub fn qf_set_session(frame: isize) {
    QF_FRAME.store(frame, Ordering::SeqCst);
    QF_ACTIVE.store(true, Ordering::SeqCst);
}

/// 逻辑线程结束会话时清位。
pub fn qf_clear_session() {
    QF_ACTIVE.store(false, Ordering::SeqCst);
    QF_FRAME.store(0, Ordering::SeqCst);
}

type EvtSender = std::sync::mpsc::Sender<KeyEvt>;
static SENDER: Mutex<Option<EvtSender>> = Mutex::new(None);

pub fn evt_channel() -> std::sync::mpsc::Receiver<KeyEvt> {
    let (tx, rx) = std::sync::mpsc::channel();
    *SENDER.lock().unwrap() = Some(tx);
    rx
}

fn send(evt: KeyEvt) {
    if let Some(tx) = SENDER.lock().unwrap().as_ref() {
        let _ = tx.send(evt);
    }
}

#[cfg(windows)]
mod platform {
    use super::{
        is_visible, qf_set_session, send, KeyEvt, QF_ACTIVE, QF_ENABLED, QF_FRAME, FJ_PICKER,
        FJ_TYPE_PASSTHROUGH, REPLACE_WIN_V, WIN_V_INTERCEPTED,
    };

    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE,
        VK_HOME, VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_RWIN, VK_LWIN,
        VK_SHIFT, VK_TAB, VK_UP,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
        KBDLLHOOKSTRUCT_FLAGS, LLKHF_INJECTED, WH_KEYBOARD_LL,
    };

    const WM_KEYDOWN: usize = 0x0100;
    const WM_KEYUP: usize = 0x0101;
    const WM_SYSKEYDOWN: usize = 0x0104;
    const WM_SYSKEYUP: usize = 0x0105;
    /// F2（重命名）：记录按下时刻，短时间内抑制会话启动
    const VK_F2: u32 = 0x71;

    static HOOK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

    /// F2 抑制窗口（对齐 WPF：重命名编辑框迟到就绪期间不吞字符）。
    /// ms 计时锚点 = 进程启动时刻（单调，无 TickCount 回绕问题）。
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    static LAST_F2_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static LAST_F2_FG: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
    static ALT_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static ALT_COMBO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn now_ms() -> u64 {
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // 存活心跳：前 3 次全记，之后每 500 次记一次（调用停涨 = 被系统摘钩）。
        {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n < 3 || n % 500 == 0 {
                crate::win_popup::append_debug_log(
                    "hotkey_debug.log",
                    &format!("kbd hook alive #{n} code={code} wp=0x{:X}", wparam.0),
                );
            }
        }
        if code == 0 {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let down = wparam.0 == WM_KEYDOWN || wparam.0 == WM_SYSKEYDOWN;
            let up = wparam.0 == WM_KEYUP || wparam.0 == WM_SYSKEYUP;
            if down || up {
                update_pt_latch(kb.vkCode, down);
                super::note_modifier(kb.vkCode, down);
            }
            if intercept_win_v(kb, down, up) {
                return LRESULT(1);
            }
        }
        if code == 0 && wparam.0 == WM_KEYUP && super::BATCH_WATCH.load(Ordering::SeqCst) && !is_visible()
        {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if kb.flags & LLKHF_INJECTED == KBDLLHOOKSTRUCT_FLAGS(0) && is_paste_keyup(kb.vkCode) {
                send(KeyEvt::BatchAdvance);
            }
        }
        if code == 0 && (wparam.0 == WM_KEYDOWN || wparam.0 == WM_SYSKEYDOWN) {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            // 录制期诊断：键是否进钩子、路由被谁拿走（visible 优先吞）。
            if super::recording_slot() >= 0 {
                crate::win_popup::append_debug_log(
                    "hotkey_debug.log",
                    &format!(
                        "rec hook vk=0x{:02X} {} visible={} qf_active={}",
                        kb.vkCode,
                        if wparam.0 == WM_SYSKEYDOWN { "sys" } else { "key" },
                        is_visible(),
                        QF_ACTIVE.load(Ordering::SeqCst),
                    ),
                );
            }
            // SendInput 注入的键必须放行（粘贴前抬 Ctrl、随后 Shift+Insert）。
            // 钉住面板时钩子仍在，不放行会把模拟粘贴吃掉。
            if kb.flags & LLKHF_INJECTED == KBDLLHOOKSTRUCT_FLAGS(0) {
            if is_visible() {
                if FJ_PICKER.load(Ordering::SeqCst) {
                    if FJ_TYPE_PASSTHROUGH.load(Ordering::SeqCst) {
                        // 用户已点对话框输入框：放行，方便改文件名
                    } else if handle_alt_down(kb.vkCode) {
                        return LRESULT(1);
                    } else if let Some(evt) = translate(kb.vkCode) {
                        if ALT_ARMED.load(Ordering::SeqCst) {
                            ALT_COMBO.store(true, Ordering::SeqCst);
                        }
                        send(evt);
                        return LRESULT(1);
                    } else if !super::is_passthrough_mod_vk(kb.vkCode) && !ctrl_or_alt_or_win_down()
                    {
                        return LRESULT(1);
                    }
                } else if explorer_fg_passthrough() {
                    // 剪贴板仍显示但焦点在 Explorer：不吞，交给 QF / 资源管理器
                    if super::QF_ENABLED.load(Ordering::SeqCst) && qf_handle(kb) {
                        return LRESULT(1);
                    }
                } else if super::live_should_passthrough(kb.vkCode) {
                    // 放行给前台应用（WPF KeyPassthroughHelper）
                } else if swallow_edit_key(kb.vkCode) {
                    return LRESULT(1);
                } else if super::edit_mode() != 0 {
                    // 编辑浮层：其余键放行给 TextInput / IME
                } else if handle_alt_down(kb.vkCode) {
                    return LRESULT(1);
                } else if let Some(evt) = translate(kb.vkCode) {
                    if ALT_ARMED.load(Ordering::SeqCst) {
                        ALT_COMBO.store(true, Ordering::SeqCst);
                    }
                    send(evt);
                    // 吞掉，避免漏给前台应用
                    return LRESULT(1);
                }
            } else if super::recording_slot() >= 0 {
                // 设置窗口热键录制：KEYDOWN + SYSKEYDOWN 全吞（Alt 组合走 SYSKEYDOWN，
                // 旧代码只认 KEYDOWN 导致 Ctrl+Alt+V 这类永远采不到），原始码 + 修饰快照上报
                //（含 Esc=取消，纯修饰由逻辑层忽略）。
                if !is_modifier_vk(kb.vkCode) {
                    send(KeyEvt::RecordVk(kb.vkCode, current_modifiers()));
                }
                return LRESULT(1);
            } else if wparam.0 == WM_KEYDOWN && QF_ENABLED.load(Ordering::SeqCst) {
                // 快速查找：仅 WM_KEYDOWN（对齐 WPF；Alt 组合走 SYSKEYDOWN 天然放行）
                if qf_handle(kb) {
                    return LRESULT(1);
                }
            }
            }
        }
        if code == 0 && (wparam.0 == WM_KEYUP || wparam.0 == WM_SYSKEYUP) && is_visible() {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if kb.flags & LLKHF_INJECTED == KBDLLHOOKSTRUCT_FLAGS(0) && handle_alt_up(kb.vkCode) {
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

    unsafe fn intercept_win_v(kb: &KBDLLHOOKSTRUCT, down: bool, up: bool) -> bool {
        if !REPLACE_WIN_V.load(Ordering::SeqCst) {
            return false;
        }
        if kb.flags & LLKHF_INJECTED != KBDLLHOOKSTRUCT_FLAGS(0) {
            return false;
        }
        if down && kb.vkCode == 0x56 {
            let win = key_down(VK_LWIN) || key_down(VK_RWIN);
            let ctrl = key_down(VK_CONTROL);
            if win && !ctrl {
                WIN_V_INTERCEPTED.store(true, Ordering::SeqCst);
                send(KeyEvt::WinV);
                return true;
            }
        }
        if up && WIN_V_INTERCEPTED.load(Ordering::SeqCst) && (kb.vkCode == 0x5B || kb.vkCode == 0x5C)
        {
            WIN_V_INTERCEPTED.store(false, Ordering::SeqCst);
            inject_win_keyup_reset(kb.vkCode);
            return true;
        }
        false
    }

    unsafe fn inject_win_keyup_reset(vk: u32) {
        fn key(vk: VIRTUAL_KEY, flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS) -> INPUT {
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
        let win = VIRTUAL_KEY(vk as u16);
        let inputs = [
            key(VK_ESCAPE, Default::default()),
            key(VK_ESCAPE, KEYEVENTF_KEYUP),
            key(win, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY),
        ];
        let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }

    fn swallow_edit_key(vk: u32) -> bool {
        let mode = super::edit_mode();
        if mode == 0 {
            return false;
        }
        if vk == 0x1B {
            send(KeyEvt::Esc);
            return true;
        }
        if vk == 0x0D {
            let ctrl = unsafe { key_down(VK_CONTROL) };
            if mode == 2 {
                send(KeyEvt::Enter);
                return true;
            }
            if mode == 1 && ctrl {
                send(KeyEvt::CtrlEnter);
                return true;
            }
        }
        false
    }

    fn handle_alt_down(vk: u32) -> bool {
        if vk != 0x12 && vk != 0xA4 && vk != 0xA5 {
            return false;
        }
        unsafe {
            if key_down(VK_CONTROL) || key_down(VK_LWIN) || key_down(VK_RWIN) {
                return false;
            }
        }
        ALT_ARMED.store(true, Ordering::SeqCst);
        ALT_COMBO.store(false, Ordering::SeqCst);
        true
    }

    fn handle_alt_up(vk: u32) -> bool {
        if vk != 0x12 && vk != 0xA4 && vk != 0xA5 {
            return false;
        }
        if !ALT_ARMED.swap(false, Ordering::SeqCst) {
            return false;
        }
        let combo = ALT_COMBO.swap(false, Ordering::SeqCst);
        if !combo {
            send(KeyEvt::AltTap);
        }
        true
    }

    fn explorer_fg_passthrough() -> bool {
        unsafe {
            let fg = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
            if fg.0.is_null() {
                return false;
            }
            crate::explorer_shell::find_cabinet_frame(fg.0 as isize).is_some()
                || crate::explorer_shell::is_desktop_hwnd(fg.0 as isize)
        }
    }

    // ===================== 快速查找钩子快路径（<1ms，仅 Win32） =====================

    /// 返回 true = 吞键；false = 放行。事件直接投递逻辑线程。
    unsafe fn qf_handle(kb: &KBDLLHOOKSTRUCT) -> bool {
        // 注入键放行（密码管理器/自身 SendInput 等）
        if kb.flags & LLKHF_INJECTED != KBDLLHOOKSTRUCT_FLAGS(0) {
            return false;
        }
        let fg = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        if fg.0.is_null() {
            return false;
        }
        let mut fg_pid: u32 = 0;
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(fg, Some(&mut fg_pid));
        if fg_pid == std::process::id() {
            // 自己的窗口前台：会话中仅 Esc 结束（对齐 WPF）
            if QF_ACTIVE.load(Ordering::SeqCst) && kb.vkCode == VK_ESCAPE.0 as u32 {
                send(KeyEvt::QfEnd);
                return true;
            }
            return false;
        }

        // F2 重命名追踪（Explorer 焦点移到重命名框有延迟）
        if kb.vkCode == VK_F2 {
            LAST_F2_MS.store(now_ms(), Ordering::SeqCst);
            LAST_F2_FG.store(fg.0 as isize, Ordering::SeqCst);
        }

        if QF_ACTIVE.load(Ordering::SeqCst) {
            qf_session_key(fg, kb)
        } else {
            qf_try_start(fg, kb)
        }
    }

    /// 会话中：全面接管键盘，防止 Explorer 处理任何按键。
    unsafe fn qf_session_key(fg: HWND, kb: &KBDLLHOOKSTRUCT) -> bool {
        let frame = QF_FRAME.load(Ordering::SeqCst);
        // 前台已离开目标资源管理器 → 结束会话并放行
        if !still_target_explorer(frame, fg) {
            send(KeyEvt::QfEnd);
            return false;
        }
        // 焦点落入可编辑控件（F2 迟到/地址栏/搜索框）→ 结束并放行
        if crate::explorer_shell::focus_is_edit_box(frame) {
            send(KeyEvt::QfEnd);
            return false;
        }
        // 修饰键本身吞掉（会话期间修饰键状态仍可被 GetAsyncKeyState 观测）
        if is_modifier_vk(kb.vkCode) {
            return true;
        }

        let ctrl = key_down(VK_CONTROL);
        let alt = key_down(VK_MENU);
        let win = key_down(VK_LWIN) || key_down(VK_RWIN);

        // Ctrl+1..9：快选对应行
        if ctrl && !alt && !win && (0x31..=0x39).contains(&kb.vkCode) {
            send(KeyEvt::QfQuickSelect((kb.vkCode - 0x31) as u8));
            return true;
        }
        // 其它修饰键组合：结束会话并放行（热键/系统快捷键可用）
        if ctrl || alt || win {
            send(KeyEvt::QfEnd);
            return false;
        }

        match kb.vkCode as u16 {
            v if v == VK_ESCAPE.0 => send(KeyEvt::QfEsc),
            v if v == VK_RETURN.0 => send(KeyEvt::QfEnter),
            v if v == VK_UP.0 => send(KeyEvt::QfUp),
            v if v == VK_DOWN.0 => send(KeyEvt::QfDown),
            v if v == VK_LEFT.0 => send(KeyEvt::QfPage(-1)),
            v if v == VK_RIGHT.0 => send(KeyEvt::QfPage(1)),
            v if v == VK_BACK.0 => send(KeyEvt::QfBackspace),
            0x21 => send(KeyEvt::QfPage(-1)), // Page Up
            0x22 => send(KeyEvt::QfPage(1)),  // Page Down
            v if v == VK_HOME.0 => send(KeyEvt::QfHome),
            v if v == VK_END.0 => send(KeyEvt::QfEndKey),
            v if v == VK_DELETE.0 => return true, // Delete：忽略（对齐 WPF）
            _ => {
                // 可打印字符入检索词，其余键吞掉
                if let Some(c) = char_for_qf(kb.vkCode) {
                    if c >= ' ' {
                        send(KeyEvt::QfChar(c));
                    }
                }
                return true;
            }
        }
        true
    }

    /// 空闲：判断是否在 Explorer/桌面文件列表上下文打字，是则吞键并启动会话。
    unsafe fn qf_try_start(fg: HWND, kb: &KBDLLHOOKSTRUCT) -> bool {
        let vk = kb.vkCode;
        // 导航/功能键等不触发会话（对齐 WPF TryStartSessionInHook 排除表）
        if matches!(
            vk,
            0x1B | 0x0D | 0x08 | 0x25 | 0x26 | 0x27 | 0x28 | 0x2E | 0x09 | 0x91 | 0x90 | 0x2C | 0x20
        ) || (0x70..=0x87).contains(&vk)
        {
            return false;
        }
        if is_modifier_vk(vk) {
            return false;
        }
        if ctrl_or_alt_or_win_down() {
            return false;
        }

        let is_desktop = crate::explorer_shell::is_desktop_hwnd(fg.0 as isize);
        let frame = if is_desktop {
            fg
        } else {
            match crate::explorer_shell::find_cabinet_frame(fg.0 as isize) {
                Some(f) => HWND(f as *mut _),
                None => return false,
            }
        };
        // 焦点在编辑框（地址栏/搜索框/重命名）→ 不启动
        if crate::explorer_shell::focus_is_edit_box(frame.0 as isize) {
            return false;
        }

        // F2 刚在同一窗口按下：字符留给重命名编辑框
        let last_ms = LAST_F2_MS.load(Ordering::SeqCst);
        if last_ms != 0 {
            let since = now_ms().saturating_sub(last_ms);
            let last_fg = LAST_F2_FG.load(Ordering::SeqCst);
            if since < 1500 && (last_fg == fg.0 as isize || last_fg == frame.0 as isize) {
                return false;
            }
        }

        let Some(ch) = char_for_qf(vk) else {
            return false;
        };
        if ch <= ' ' {
            return false;
        }

        // 吞键并启动会话（hook 侧先置位，逻辑线程异步解析文件夹）
        qf_set_session(frame.0 as isize);
        send(KeyEvt::QfStart {
            frame: frame.0 as isize,
            desktop: is_desktop,
            ch,
        });
        true
    }

    /// 会话中快速判断前台是否仍是目标资源管理器（对齐 WPF IsStillTargetExplorer）。
    unsafe fn still_target_explorer(frame: isize, fg: HWND) -> bool {
        if frame == 0 {
            return false;
        }
        if fg.0 as isize == frame {
            return true;
        }
        crate::explorer_shell::find_cabinet_frame(fg.0 as isize) == Some(frame)
    }

    /// WPF IsModifierKey：Shift/Ctrl/Alt/CapsLock/左右修饰/Win。
    fn is_modifier_vk(vk: u32) -> bool {
        matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0xA0..=0xA5 | 0x5B | 0x5C)
    }

    /// 快速查找字符：复用弹窗 VK→char 表（数字作为字符）。
    /// 空格不作为会话首字符（桌面/资源管理器空格是选中，不是打字）。
    fn char_for_qf(vk: u32) -> Option<char> {
        if vk == 0x20 {
            return Some(' ');
        }
        match char_from_vk(vk as u16, shift_down()) {
            Some(KeyEvt::Char(c)) => Some(c),
            Some(KeyEvt::Digit(n)) => Some((b'0' + n) as char),
            _ => None,
        }
    }

    unsafe fn key_down(vk: VIRTUAL_KEY) -> bool {
        (GetAsyncKeyState(vk.0 as i32) as u16) & 0x8000 != 0
    }

    fn shift_down() -> bool {
        unsafe { (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16) & 0x8000 != 0 }
    }

    unsafe fn is_paste_keyup(vk: u32) -> bool {
        // Ctrl+V 或 Shift+Insert（无 Ctrl，避免和 Ctrl+Shift+Ins 冲突）
        if vk == 0x56 && key_down(VK_CONTROL) && !key_down(VK_MENU) {
            return true;
        }
        if vk == 0x2D && key_down(VK_SHIFT) && !key_down(VK_CONTROL) {
            return true;
        }
        false
    }

    fn ctrl_or_alt_or_win_down() -> bool {
        unsafe {
            ((GetAsyncKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0)
                || ((GetAsyncKeyState(VK_MENU.0 as i32) as u16) & 0x8000 != 0)
                || ((GetAsyncKeyState(VK_LWIN.0 as i32) as u16) & 0x8000 != 0)
                || ((GetAsyncKeyState(VK_RWIN.0 as i32) as u16) & 0x8000 != 0)
        }
    }

    /// 当前修饰键状态（WPF RegisterHotKey 原值；CapsLock 读物理按下）。
    pub fn current_modifiers() -> u32 {
        unsafe {
            let mut m = 0u32;
            if key_down(VK_CONTROL) {
                m |= crate::settings::MOD_CONTROL;
            }
            if key_down(VK_MENU) {
                m |= crate::settings::MOD_ALT;
            }
            if key_down(VK_SHIFT) {
                m |= crate::settings::MOD_SHIFT;
            }
            if key_down(VK_LWIN) || key_down(VK_RWIN) {
                m |= crate::settings::MOD_WIN;
            }
            if (GetAsyncKeyState(0x14 as i32) as u16) & 0x8000 != 0 {
                m |= crate::settings::MOD_CAPS;
            }
            m
        }
    }

    fn panel_down() -> bool {
        unsafe {
            match crate::policy::panel_vk() {
                0x12 => key_down(VK_MENU),
                0x5B => key_down(VK_LWIN) || key_down(VK_RWIN),
                0x14 => (GetAsyncKeyState(0x14 as i32) as u16) & 0x8000 != 0,
                _ => key_down(VK_CONTROL),
            }
        }
    }

    /// 返回 None = 不拦截（修饰键本身 / 修饰键组合 / 不支持的键）。
    fn translate(vk: u32) -> Option<KeyEvt> {
        // 呼出热键必须先于「Ctrl 组合放行」：否则 Ctrl+` 的 ` 会进搜索框。
        let mods = current_modifiers();
        let clip = crate::settings::Hotkey::new(
            super::CLIP_MOD.load(Ordering::SeqCst),
            super::CLIP_VK.load(Ordering::SeqCst),
        );
        let fj = crate::settings::Hotkey::new(
            super::FJ_MOD.load(Ordering::SeqCst),
            super::FJ_VK.load(Ordering::SeqCst),
        );
        let batch = crate::settings::Hotkey::new(
            super::BATCH_MOD.load(Ordering::SeqCst),
            super::BATCH_VK.load(Ordering::SeqCst),
        );
        // CapsLock 物理按下会进 current_modifiers，RegisterHotKey 不认它，匹配时去掉。
        let mods_hot = mods & !crate::settings::MOD_CAPS;
        if clip.matches(mods, vk) || clip.matches(mods_hot, vk) {
            return Some(KeyEvt::Toggle);
        }
        if fj.matches(mods, vk) || fj.matches(mods_hot, vk) {
            return Some(KeyEvt::FileJumpHotkey);
        }
        if batch.matches(mods, vk) || batch.matches(mods_hot, vk) {
            return Some(KeyEvt::BatchHotkey);
        }
        // Ctrl+P：切换选中条目置顶（仅 Ctrl，无 Alt/Win，避免吞掉系统组合）
        if ctrl_only_down() && vk == 0x50 {
            return Some(KeyEvt::PinToggle);
        }
        // 面板主键组合（WPF PanelModifierKey）：m+N 快贴、m+Tab 短语过滤。
        // 注意：主键=Ctrl 时 Ctrl+P 上已优先；此处处理数字与 Tab。
        if panel_down() {
            match vk {
                0x30..=0x39 => return Some(KeyEvt::QuickNum((vk - 0x30) as u8)),
                0x60..=0x69 => return Some(KeyEvt::QuickNum((vk - 0x60) as u8)),
                0x09 => return Some(KeyEvt::QuickTab),
                _ => {}
            }
        }
        // 自定义翻页（默认同 WPF：Ctrl+- / Ctrl+=），须在修饰键放行之前匹配。
        if crate::settings::Hotkey::new(
            super::PAGE_UP_MOD.load(Ordering::SeqCst),
            super::PAGE_UP_VK.load(Ordering::SeqCst),
        )
        .matches(mods, vk)
        {
            return Some(KeyEvt::PgUp);
        }
        if crate::settings::Hotkey::new(
            super::PAGE_DN_MOD.load(Ordering::SeqCst),
            super::PAGE_DN_VK.load(Ordering::SeqCst),
        )
        .matches(mods, vk)
        {
            return Some(KeyEvt::PgDn);
        }
        if vk == VK_RETURN.0 as u32 && ctrl_only_down() {
            return Some(KeyEvt::CtrlEnter);
        }
        unsafe {
            if vk == VK_RETURN.0 as u32
                && shift_down()
                && !key_down(VK_CONTROL)
                && !key_down(VK_MENU)
            {
                return Some(KeyEvt::ShiftEnter);
            }
            if shift_down() && !key_down(VK_CONTROL) && !key_down(VK_MENU) {
                if vk == VK_UP.0 as u32 {
                    return Some(KeyEvt::ShiftUp);
                }
                if vk == VK_DOWN.0 as u32 {
                    return Some(KeyEvt::ShiftDown);
                }
            }
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
            VK_UP => Some(KeyEvt::Up),
            VK_DOWN => Some(KeyEvt::Down),
            VK_LEFT => Some(KeyEvt::Left),
            VK_RIGHT => Some(KeyEvt::Right),
            VK_TAB => Some(KeyEvt::Tab),
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
                // 数字：始终进搜索（WPF VkToChar）；快贴只走面板主键+1..9
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

    fn vk_to_mod_bit(vk: u32) -> u32 {
        match vk {
            0x14 => crate::settings::MOD_CAPS,
            0x10 | 0xA0 | 0xA1 => crate::settings::MOD_SHIFT,
            0x11 | 0xA2 | 0xA3 => crate::settings::MOD_CONTROL,
            0x12 | 0xA4 | 0xA5 => crate::settings::MOD_ALT,
            0x5B | 0x5C => crate::settings::MOD_WIN,
            _ => 0,
        }
    }

    fn family_physically_down(bit: u32) -> bool {
        unsafe {
            match bit {
                crate::settings::MOD_CONTROL => {
                    key_down(VK_CONTROL)
                        || (GetAsyncKeyState(0xA2) as u16) & 0x8000 != 0
                        || (GetAsyncKeyState(0xA3) as u16) & 0x8000 != 0
                }
                crate::settings::MOD_SHIFT => {
                    key_down(VK_SHIFT)
                        || (GetAsyncKeyState(0xA0) as u16) & 0x8000 != 0
                        || (GetAsyncKeyState(0xA1) as u16) & 0x8000 != 0
                }
                crate::settings::MOD_ALT => {
                    key_down(VK_MENU)
                        || (GetAsyncKeyState(0xA4) as u16) & 0x8000 != 0
                        || (GetAsyncKeyState(0xA5) as u16) & 0x8000 != 0
                }
                crate::settings::MOD_WIN => key_down(VK_LWIN) || key_down(VK_RWIN),
                crate::settings::MOD_CAPS => (GetAsyncKeyState(0x14) as u16) & 0x8000 != 0,
                _ => false,
            }
        }
    }

    fn update_pt_latch(vk: u32, down: bool) {
        let bit = vk_to_mod_bit(vk);
        if bit == 0 {
            return;
        }
        let mut latch = super::PT_LATCH.load(Ordering::SeqCst);
        if down {
            if super::PT_CFG.lock().unwrap().enabled {
                latch |= bit;
            }
        } else {
            latch &= !bit;
            if family_physically_down(bit) {
                latch |= bit;
            }
        }
        super::PT_LATCH.store(latch, Ordering::SeqCst);
    }

    /// 必须在事件循环线程调用（WH_KEYBOARD_LL 依赖安装线程的消息循环）。
    pub fn install() -> bool {
        if HOOK.load(Ordering::SeqCst) != 0 {
            return true;
        }
        unsafe {
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
                Ok(handle) => {
                    HOOK.store(handle.0 as isize, Ordering::SeqCst);
                    crate::win_popup::append_debug_log(
                        "hotkey_debug.log",
                        &format!("kbd hook installed h=0x{:X}", handle.0 as isize),
                    );
                    true
                }
                Err(e) => {
                    crate::win_popup::append_debug_log(
                        "hotkey_debug.log",
                        &format!("kbd hook install FAILED: {e}"),
                    );
                    false
                }
            }
        }
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
pub use platform::{current_modifiers, install, uninstall};

/// 重装钩子（看门狗/显隐时调用）：Slint 主线程偶发长阻塞（全量推行）会被系统
/// 静默摘钩且不报错；重装存活钩只是走一遍链（微秒级），摘掉的则复活。
/// 必须在事件循环线程调用。
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
    pub fn uninstall() {}

    pub fn current_modifiers() -> u32 {
        0
    }
}

#[cfg(not(windows))]
pub use fallback::{current_modifiers, install, uninstall};

fn live_should_passthrough(vk: u32) -> bool {
    let cfg = PT_CFG.lock().unwrap().clone();
    let latch = PT_LATCH.load(Ordering::SeqCst) | current_modifiers();
    should_passthrough(&cfg, latch, vk)
}

fn is_essential_panel_key(vk: u32) -> bool {
    matches!(
        vk,
        0x1B | 0x0D | 0x08 | 0x2E | 0x25 | 0x26 | 0x27 | 0x28 | 0x24 | 0x23 | 0x21 | 0x22
    )
}

fn is_passthrough_mod_vk(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

fn family_held(mods: u32, bit: u32) -> bool {
    mods & bit != 0
}

fn mask_matches_exact(required: u32, held: u32) -> bool {
    use crate::settings::{MOD_ALT, MOD_CAPS, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
    family_held(held, MOD_CONTROL) == family_held(required, MOD_CONTROL)
        && family_held(held, MOD_SHIFT) == family_held(required, MOD_SHIFT)
        && family_held(held, MOD_ALT) == family_held(required, MOD_ALT)
        && family_held(held, MOD_WIN) == family_held(required, MOD_WIN)
        && family_held(held, MOD_CAPS) == family_held(required, MOD_CAPS)
}

fn any_mask_held(mask: u32, held: u32) -> bool {
    mask != 0 && (mask & held) != 0
}

fn should_passthrough(cfg: &PassthroughCfg, held: u32, vk: u32) -> bool {
    if !cfg.enabled || is_passthrough_mod_vk(vk) {
        return false;
    }
    if cfg.keep_panel && is_essential_panel_key(vk) {
        return false;
    }
    if !cfg.rules.is_empty() {
        for r in &cfg.rules {
            if mask_matches_exact(r.modifiers, held) && (r.key == 0 || r.key == vk) {
                return true;
            }
        }
    }
    any_mask_held(cfg.mask, held)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{PassthroughRule, MOD_CAPS, MOD_CONTROL};

    fn cfg(enabled: bool, mask: u32, keep: bool, rules: Vec<PassthroughRule>) -> PassthroughCfg {
        PassthroughCfg {
            enabled,
            mask,
            keep_panel: keep,
            rules,
        }
    }

    #[test]
    fn passthrough_off_never_leaks() {
        let c = cfg(false, MOD_CAPS, true, vec![]);
        assert!(!should_passthrough(&c, MOD_CAPS, 0x41));
    }

    #[test]
    fn passthrough_caps_letter_but_keeps_esc() {
        let c = cfg(true, MOD_CAPS, true, vec![]);
        assert!(should_passthrough(&c, MOD_CAPS, 0x41));
        assert!(!should_passthrough(&c, MOD_CAPS, 0x1B));
        assert!(!should_passthrough(&c, 0, 0x41));
    }

    #[test]
    fn passthrough_rule_exact_and_wildcard() {
        let exact = cfg(
            true,
            0,
            true,
            vec![PassthroughRule {
                modifiers: MOD_CONTROL,
                key: 0x43,
            }],
        );
        assert!(should_passthrough(&exact, MOD_CONTROL, 0x43));
        assert!(!should_passthrough(&exact, MOD_CONTROL, 0x56));
        let wild = cfg(
            true,
            0,
            true,
            vec![PassthroughRule {
                modifiers: MOD_CONTROL,
                key: 0,
            }],
        );
        assert!(should_passthrough(&wild, MOD_CONTROL, 0x56));
        assert!(!should_passthrough(&wild, 0, 0x56));
    }

    #[test]
    fn click_ctrl_ignores_held_key_until_release_after_arm() {
        note_modifier(0x11, true);
        arm_click_modifiers();
        assert!(!click_ctrl());
        note_modifier(0x11, false);
        assert!(!click_ctrl());
        note_modifier(0x11, true);
        assert!(click_ctrl());
        note_modifier(0x11, false);
        assert!(!click_ctrl());
        note_modifier(0x10, true);
        arm_click_modifiers();
        assert!(!click_shift());
        note_modifier(0x10, false);
        note_modifier(0x10, true);
        assert!(click_shift());
        note_modifier(0x10, false);
    }
}
