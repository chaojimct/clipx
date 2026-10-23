//! 路线乙：按需注入 —— 在目标进程内自主测量 caret（Windows）。
//!
//! 与路线甲（注册成 TSF TIP profile）的本质区别：**全程不碰输入法**。
//! 不调用任何 profile API，不销毁/重建用户的 TIP 实例，不动 keyboard
//! openclose compartment ⇒ 微信输入法的中/英态（存在它自己实例内存里）
//! 原样保留；也不往 HKLM 注册，Win+Space 输入法列表里不会多出一项。
//!
//! 流程（按需 + 钩子短驻留，平衡"零常驻"与"首次慢"）：
//! 1. 取当前焦点窗口线程（Electron 的 TSF 上下文挂在浏览器 UI 线程上）。
//! 2. `SetWindowsHookExW(WH_GETMESSAGE, …, target_tid)` 把信标 DLL 注入该进程；
//!    钩子回调 `ClipxCaretHookProc` 在**宿主 UI 线程内**跑一次只读测量。
//! 3. `PostThreadMessageW` 唤醒目标线程，等共享内存里出现本 pid 的新结果。
//! 4. 钩子**驻留 `HOOK_GRACE_MS`**（默认 5s）：同一应用连续呼出不重复装钩，
//!    首次注入的 DLL 加载成本只付一次；过期或换目标进程时自动摘除。
//!
//! 失败一律静默返回 None，由 `win_popup` 的老链路兜底。

use clipx_core::tsf_caret as proto;
use std::sync::Mutex;
use std::time::Instant;

/// 诊断日志：写 `%TEMP%\clipx_inject.log`，用于定位注入失败原因。
fn ilog(msg: &str) {
    use std::io::Write;
    use windows::Win32::System::SystemInformation::GetTickCount64;
    let tick = unsafe { GetTickCount64() };
    let path = std::env::temp_dir().join("clipx_inject.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{tick}ms] {msg}");
    }
}

/// 钩子驻留时长（毫秒）：同一目标线程在此窗口内重复测量不再装钩。
const HOOK_GRACE_MS: u64 = 5_000;
/// 等目标线程应答的总超时（毫秒）——**首次装钩**（DLL 要进目标进程）。
const WAIT_TIMEOUT_FRESH_MS: u64 = 400;
/// 等目标线程应答的总超时（毫秒）——钩子已驻留（热路径）。
const WAIT_TIMEOUT_WARM_MS: u64 = 80;
/// 轮询间隔（毫秒）。
const POLL_MS: u64 = 4;
/// 结果新鲜度上限（毫秒），与 tsf_caret 侧保持一致。
const MAX_AGE_MS: u64 = 1500;

/// 驻留钩子缓存：同一 UI 线程短时间内的重复测量直接复用，省掉重复装钩。
struct HookEntry {
    hook: windows::Win32::UI::WindowsAndMessaging::HHOOK,
    tid: u32,
    /// 装钩时的目标进程：排查「同一 tid 换进程」用，当前只落日志不参与判定。
    #[allow(dead_code)]
    pid: u32,
    installed_at: Instant,
}

// HHOOK 是内核句柄（非指针），跨线程持有没有数据竞争。
unsafe impl Send for HookEntry {}
unsafe impl Sync for HookEntry {}

impl Drop for HookEntry {
    fn drop(&mut self) {
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx;
            let _ = UnhookWindowsHookEx(self.hook);
        }
    }
}

static HOOK_CACHE: Mutex<Option<HookEntry>> = Mutex::new(None);

/// 测不出 caret 的进程黑名单（如 WXWork 等 CEF 应用：钩子挂上了但
/// TSF edit session 永远测不出，每次呼出白等 80-400ms）。
/// 连续 BEACON_FAIL_LIMIT 次超时后，BEACON_FAIL_COOLDOWN 内直接跳过注入。
const BEACON_FAIL_LIMIT: u32 = 2;
const BEACON_FAIL_COOLDOWN_MS: u128 = 60_000;
static BEACON_FAIL: Mutex<Option<(u32, u32, Instant)>> = Mutex::new(None); // (pid, fails, last_fail)

fn beacon_blacklisted(pid: u32) -> bool {
    let Ok(mut guard) = BEACON_FAIL.lock() else {
        return false;
    };
    match *guard {
        Some((p, fails, since)) if p == pid => {
            if fails >= BEACON_FAIL_LIMIT {
                if since.elapsed().as_millis() < BEACON_FAIL_COOLDOWN_MS {
                    return true;
                }
                // 冷却结束：清零重试（应用可能换了输入框）。
                *guard = Some((pid, 0, Instant::now()));
            }
            false
        }
        _ => false,
    }
}

fn beacon_note_fail(pid: u32) {
    if let Ok(mut guard) = BEACON_FAIL.lock() {
        match *guard {
            Some((p, fails, _)) if p == pid => {
                *guard = Some((pid, fails + 1, Instant::now()));
            }
            _ => {
                *guard = Some((pid, 1, Instant::now()));
            }
        }
    }
}

fn beacon_note_ok(pid: u32) {
    if let Ok(mut guard) = BEACON_FAIL.lock() {
        if matches!(*guard, Some((p, _, _)) if p == pid) {
            *guard = None;
        }
    }
}

/// 注入模式下的测量入口。
///
/// 返回 `(CaretRect, tag)`；拿不到就 `None`，由调用方回落老链路。
pub fn measure_foreground_caret() -> Option<(proto::CaretRect, u32)> {
    let (target_tid, target_pid) = focus_thread()?;
    ilog(&format!(
        "measure_fg: tid={target_tid} pid={target_pid}"
    ));
    measure_thread_caret(target_tid, target_pid)
}

/// 对指定 pid 的 UI 线程做一次注入测量（自检用，不依赖抢焦点）。
///
/// 与 `measure_foreground_caret` 同一条链路，只是目标线程由调用方指定。
pub fn measure_pid_caret(target_pid: u32) -> Option<(proto::CaretRect, u32)> {
    if target_pid == 0 || target_pid == std::process::id() {
        return None;
    }
    let target_tid = ui_thread_of(target_pid)?;
    measure_thread_caret(target_tid, target_pid)
}

/// 目标进程是否 32 位（WOW64）。
///
/// 本进程是 x64 时，32 位目标无法加载 x64 信标 DLL；这种进程一次都不可能成功，
/// 与其每次呼出白等 400ms 再进黑名单，不如一次问清直接跳过。
fn target_is_32bit(pid: u32) -> bool {
    #[cfg(not(windows))]
    {
        let _ = pid;
        return false;
    }
    #[cfg(windows)]
    {
        // 仅当自己是 64 位时才有「x64 DLL 进不去 32 位进程」的问题。
        if cfg!(target_pointer_width = "32") {
            return false;
        }
        use windows::Win32::Foundation::{CloseHandle, BOOL};
        use windows::Win32::System::Threading::{
            IsWow64Process, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut wow = BOOL(0);
            let ok = IsWow64Process(h, &mut wow).is_ok();
            let _ = CloseHandle(h);
            ok && wow.as_bool()
        }
    }
}

fn measure_thread_caret(target_tid: u32, target_pid: u32) -> Option<(proto::CaretRect, u32)> {
    if target_pid == std::process::id() {
        return None; // 自己不注入
    }
    // 黑名单先查：已确认测不出的宿主（CEF 一类）不该再付注入/装钩的代价。
    // 顺序曾经反了 —— 32 位目标先进再判黑名单，等于每次白花 130ms 重注入。
    if beacon_blacklisted(target_pid) {
        ilog(&format!(
            "measure: skip pid={target_pid} (blacklisted, beacon never works here)"
        ));
        return None;
    }
    // 32 位目标：x64 进程的 `SetWindowsHookEx` 装不进 WOW64 宿主（位数必须一致）。
    // 改走「远程注入 x86 信标 → 由 DLL 在宿主内自装钩子」。
    // 以前这里直接放弃，企业微信/WPS/腾讯会议等 32 位应用只能吃启发式落点；
    // 现在它们和 64 位应用一样能拿到像素级 caret。
    let mut is32 = false;
    let mut injected_now = false;
    if target_is_32bit(target_pid) {
        match ensure_injected_beacon(target_pid) {
            Some(fresh) => {
                is32 = true;
                injected_now = fresh;
            }
            None => {
                ilog(&format!(
                    "measure: skip pid={target_pid} (32-bit target, beacon injection failed)"
                ));
                return None;
            }
        }
    }
    let base = crate::tsf_caret::shm_init()?;
    let start_tick = now_tick();
    let _seq = crate::tsf_caret::write_request(base)?;

    // 32 位宿主走「注入 + DLL 自装钩子」，钩子常驻在宿主里，无需再从这边装。
    let warm = if is32 {
        !injected_now
    } else {
        // 复用驻留钩子，或装新的。
        ensure_hook(target_tid, target_pid)?
    };
    let wait_ms = if warm {
        WAIT_TIMEOUT_WARM_MS
    } else {
        WAIT_TIMEOUT_FRESH_MS
    };
    ilog(&format!(
        "measure: tid={target_tid} pid={target_pid} warm={warm} wait={wait_ms}ms"
    ));

    // 唤醒目标线程，让 WH_GETMESSAGE 立刻回调。
    unsafe {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
        let _ = PostThreadMessageW(target_tid, 0 /*WM_NULL*/, WPARAM(0), LPARAM(0));
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    while std::time::Instant::now() < deadline {
        if let Some(c) = crate::tsf_caret::scan_results(base, target_pid) {
            if c.tick >= start_tick && proto::is_fresh(&c, now_tick(), MAX_AGE_MS) && c.sane() {
                ilog(&format!("measure: OK rect={:?}", c.rect));
                beacon_note_ok(target_pid);
                return Some((c, target_pid));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
    }
    ilog(&format!(
        "measure: TIMEOUT pid={target_pid} (no result in {wait_ms}ms)"
    ));
    beacon_note_fail(target_pid);
    // 32 位注入缓存一旦测量失败就作废：可能是 pid 被复用（缓存里的进程已退出，
    // 新进程没装信标），也可能是信标装歪了。下次呼出重新注入比一直吃旧缓存稳。
    if is32 {
        forget_injected(target_pid);
    }
    None
}

/// 抹掉某 pid 的「已注入」记账，迫使下次重新注入。
fn forget_injected(pid: u32) {
    if let Ok(mut g) = INJECTED.lock() {
        g.retain(|(p, _)| *p != pid);
    }
}

/// 已注入信标的 32 位进程缓存：pid → 注入时刻。
///
/// 进程退出后 pid 可能被复用，所以带 TTL 兜底（不追求精确，只在过期后重注一次）。
const INJECT_TTL_MS: u128 = 600_000; // 10 分钟
static INJECTED: Mutex<Vec<(u32, Instant)>> = Mutex::new(Vec::new());

/// 确保 32 位宿主进程里已加载 x86 信标、且钩子已挂上。
///
/// 返回 `Some(true)` = 本次刚注入（首次，等待窗口要放宽）；
/// `Some(false)` = 之前注入过（热路径）；`None` = 注入失败，调用方回落启发式。
///
/// 幂等：同一 pid 在 TTL 内只注入一次。信标的 `ClipxBeaconStart` 自身也幂等，
/// 即使并发进来重复调用，也只会装一套钩子。
fn ensure_injected_beacon(pid: u32) -> Option<bool> {
    {
        let mut g = INJECTED.lock().ok()?;
        g.retain(|(p, at)| *p != pid || at.elapsed().as_millis() < INJECT_TTL_MS);
        if g.iter().any(|(p, _)| *p == pid) {
            return Some(false);
        }
    }
    let t0 = Instant::now();
    match clipx_filejump::inject::win::inject_caret_beacon(pid) {
        Ok(n) if n > 0 => {
            ilog(&format!(
                "inject_beacon: ok pid={pid} hooks={n} in {:?}",
                t0.elapsed()
            ));
            if let Ok(mut g) = INJECTED.lock() {
                if g.len() >= 16 {
                    g.remove(0);
                }
                g.push((pid, Instant::now()));
            }
            Some(true)
        }
        Ok(n) => {
            ilog(&format!(
                "inject_beacon: zero hooks pid={pid} (n={n}) in {:?}",
                t0.elapsed()
            ));
            None
        }
        Err(e) => {
            ilog(&format!("inject_beacon: failed pid={pid} err={e:?}"));
            None
        }
    }
}

/// 确保目标线程上有可用的钩子。返回 `true` 表示复用了驻留钩子（热路径）。
fn ensure_hook(target_tid: u32, target_pid: u32) -> Option<bool> {
    let mut cache = HOOK_CACHE.lock().ok()?;

    // 命中：同 tid 且未过期 → 直接复用。
    if let Some(entry) = cache.as_ref() {
        if entry.tid == target_tid
            && entry.installed_at.elapsed().as_millis() as u64 <= HOOK_GRACE_MS
        {
            ilog(&format!("ensure_hook: reuse warm tid={target_tid}"));
            return Some(true);
        }
    }

    // 未命中：先摘旧的（换目标或过期），再装新的。
    *cache = None;
    ilog(&format!("ensure_hook: installing new tid={target_tid} pid={target_pid}"));
    let hook = install_hook(target_tid)?;
    ilog(&format!("ensure_hook: installed ok tid={target_tid}"));
    *cache = Some(HookEntry {
        hook,
        tid: target_tid,
        pid: target_pid,
        installed_at: Instant::now(),
    });
    Some(false)
}

/// 找出指定进程里「拥有可见顶层窗口」的 UI 线程 id。
fn ui_thread_of(target_pid: u32) -> Option<u32> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
    };

    struct Ctx {
        pid: u32,
        tid: u32,
    }
    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == ctx.pid && tid != 0 {
            ctx.tid = tid;
            return BOOL(0); // 找到即停
        }
        BOOL(1)
    }
    let mut ctx = Ctx { pid: target_pid, tid: 0 };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    if ctx.tid == 0 {
        None
    } else {
        Some(ctx.tid)
    }
}

/// 取「当前真正持有键盘焦点」的窗口线程。
///
/// Electron 的 TSF 上下文挂在**浏览器 UI 线程**（顶层窗口线程）上，
/// 所以这里优先用 `GetGUIThreadInfo` 的焦点窗，回落到前台窗口。
fn focus_thread() -> Option<(u32, u32)> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };
    unsafe {
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(0, &mut gti).is_ok() && !gti.hwndFocus.is_invalid() {
            let mut pid = 0u32;
            let tid = GetWindowThreadProcessId(gti.hwndFocus, Some(&mut pid));
            if tid != 0 && pid != 0 {
                return Some((tid, pid));
            }
        }
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(fg, Some(&mut pid));
        if tid == 0 || pid == 0 {
            return None;
        }
        Some((tid, pid))
    }
}

/// 把信标 DLL 挂到目标线程上（`WH_GETMESSAGE`）。
///
/// 返回 `HHOOK`，由调用方（`HookEntry`）负责摘除。
fn install_hook(target_tid: u32) -> Option<windows::Win32::UI::WindowsAndMessaging::HHOOK> {
    use windows::Win32::Foundation::{FARPROC, HINSTANCE};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, WH_GETMESSAGE};

    let dll_path = beacon_dll_path()?;
    ilog(&format!("install_hook: dll={}", dll_path.display()));
    let wide: Vec<u16> = dll_path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let hmod = LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())).ok()?;
        let proc: FARPROC =
            GetProcAddress(hmod, windows::core::s!("ClipxCaretHookProc"));
        let Some(proc) = proc else {
            ilog("install_hook: GetProcAddress(ClipxCaretHookProc) failed");
            return None;
        };
        let hook_proc: windows::Win32::UI::WindowsAndMessaging::HOOKPROC =
            Some(std::mem::transmute::<_, unsafe extern "system" fn(_, _, _) -> _>(proc));

        let hhk = match SetWindowsHookExW(WH_GETMESSAGE, hook_proc, Some(HINSTANCE(hmod.0)), target_tid) {
            Ok(h) => h,
            Err(e) => {
                ilog(&format!("install_hook: SetWindowsHookExW failed tid={target_tid} err={e}"));
                return None;
            }
        };
        ilog(&format!("install_hook: ok hhk={:?}", hhk));
        Some(hhk)
    }
}

/// 信标 DLL 路径：优先 clipx.exe 同目录（安装态），回落仓库 target/release（开发态）。
fn beacon_dll_path() -> Option<std::path::PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidates = [
        exe_dir.join("clipx_caret_beacon.dll"),
        exe_dir.join("../../clipx_caret_beacon.dll"),
        exe_dir.join("../clipx_caret_beacon.dll"),
    ];
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// 自检用：把解析到的信标 DLL 路径打出来。
pub fn debug_dll_path() -> Option<std::path::PathBuf> {
    beacon_dll_path()
}

fn now_tick() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    unsafe { GetTickCount64() }
}
