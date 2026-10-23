//! 32 位测试宿主：验证「x64 clipx → 注入 x86 信标 → 宿主内自装钩子 → 测出 caret」。
//!
//! 为什么需要它：本机多数 32 位宿主（企业微信 / WPS / 腾讯会议）不方便反复重启，
//! 而这个 example 是一个可控的 WOW64 窗口 + RichEdit 输入框（TSF 宿主），
//! 可以随时起停，用于回归。
//!
//! 用法：
//! ```text
//! cargo build -p clipx-tsf --release --target i686-pc-windows-msvc --features win32-examples --example host32
//! target\i686-pc-windows-msvc\release\examples\host32.exe
//! # 另开一个终端（先点进 RichEdit 让光标在里面）：
//! clipx.exe --beacon-inject <host32 的 pid>
//! ```
//!
//! 判读：`%TEMP%\clipx_inject.log` 里出现
//! `inject_beacon: ok pid=… hooks=N`（N>0）说明注入 + 装钩子成功；
//! `%TEMP%\clipx_caret_beacon.log` 里出现 `inject hook: request_seq=… -> measuring`
//! 说明钩子回调被触发；再到 `measure: OK rect=…` 说明 TSF 拿到了真插入点。

#![cfg(windows)]

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassW, ShowWindow, TranslateMessage, CW_USEDEFAULT, MSG, SW_SHOW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_DESTROY, WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// `ES_MULTILINE`（winuser.h 0x0004）。
const ES_MULTILINE: u32 = 0x0004;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn main() {
    unsafe {
        let hmod = GetModuleHandleW(None).expect("GetModuleHandleW");
        let hinst = HINSTANCE(hmod.0);

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: w!("clipxHost32"),
            ..Default::default()
        };
        assert_ne!(RegisterClassW(&wc), 0, "RegisterClassW 失败");

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("clipxHost32"),
            w!("clipx 32-bit test host — 点进输入框打字，另开终端跑 --beacon-inject"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            760,
            460,
            None,
            None,
            Some(hinst),
            None,
        )
        .expect("CreateWindowExW");

        // RichEdit 是可靠的 TSF 宿主（普通 EDIT 在部分系统上不给 TextExt）。
        // 必须在建控件**之前**把本线程的 TSF thread manager 激活 —— 真实宿主
        // （企业微信 / WPS / Electron）都是这么做的；不激活的话 RichEdit 不会
        // 注册 TSF 上下文，注入信标只能测到 `thread mgr has no focus`。
        {
            use windows::Win32::System::Com::{
                CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
            };
            use windows::Win32::UI::TextServices::{CLSID_TF_ThreadMgr, ITfThreadMgr};
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            match CoCreateInstance::<Option<&windows::core::IUnknown>, ITfThreadMgr>(
                &CLSID_TF_ThreadMgr,
                None,
                CLSCTX_INPROC_SERVER,
            ) {
                Ok(tim) => match tim.Activate() {
                    Ok(tid) => println!("[tsf] thread mgr activated client={tid}"),
                    Err(e) => println!("[tsf] Activate 失败: {e}"),
                },
                Err(e) => println!("[tsf] CoCreateInstance(ThreadMgr) 失败: {e}"),
            }
        }
        let _ = LoadLibraryW(w!("msftedit.dll"));
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!("hello clipx"),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | ES_MULTILINE),
            12,
            12,
            720,
            380,
            Some(hwnd),
            None,
            Some(hinst),
            None,
        )
        .expect("CreateWindowExW(RichEdit)");

        let _ = ShowWindow(hwnd, SW_SHOW);
        // 抢前台 + 给 RichEdit 焦点：TSF 的 GetFocus 需要该线程既是前台、
        // 又有文档焦点，否则注进去只测到 `thread mgr has no focus`。
        let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(edit));
        // 500ms 定时器：制造持续消息流，确保 WH_GETMESSAGE 钩子有东西可挂
        // （真实应用几乎总有动画/定时消息；纯静默窗口会让钩子看起来"没装"）。
        windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 1, 500, None);

        // `--self`：从**主线程**（已在消息循环里的正常线程）调用 DLL 的自装钩子，
        // 用于对比「远程注入线程调用」为何不触发回调。
        if std::env::args().any(|a| a == "--self") {
            let dll = std::env::args()
                .skip_while(|a| a != "--self")
                .nth(1)
                .unwrap_or_else(|| {
                    // 默认取 exe 同目录：WMI/脚本启动时工作目录不可控。
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| {
                            p.parent().map(|d| {
                                d.join("clipx_caret_beacon32.dll")
                                    .to_string_lossy()
                                    .into_owned()
                            })
                        })
                        .unwrap_or_else(|| "clipx_caret_beacon32.dll".into())
                });
            let wide: Vec<u16> = dll.encode_utf16().chain([0]).collect();
            match LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())) {
                Ok(h) => {
                    let f = windows::Win32::System::LibraryLoader::GetProcAddress(
                        h,
                        windows::core::s!("ClipxBeaconStart"),
                    );
                    match f {
                        Some(f) => {
                            let start: unsafe extern "system" fn() -> i32 =
                                std::mem::transmute(f);
                            println!("[self] ClipxBeaconStart -> hooks={}", start());
                        }
                        None => println!("[self] GetProcAddress 失败"),
                    }
                }
                Err(e) => println!("[self] LoadLibrary({dll}) 失败: {e}"),
            }
        }

        println!("host32 pid={}", std::process::id());
        println!(
            "现在点进输入框，然后跑: clipx.exe --beacon-inject {}",
            std::process::id()
        );

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// 让未使用告警闭嘴：PCWSTR 在 w! 宏展开里被引用。
const _: Option<PCWSTR> = None;
