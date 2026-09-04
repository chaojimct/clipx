//! Everything WM_COPYDATA IPC 协议实现（仅 Windows）。
//!
//! 每次 `query()` 自建一个 message-only 回执窗口：投递查询包后在本线程泵消息，
//! 直到 Everything 回投结果或超时。窗口创建是微秒级操作，无需常驻客户端对象；
//! 协议本身无进程内共享状态（WPF 版的 SDK 全局锁在此不适用），可多线程并发。
//!
//! 服务端布局协商：真 Everything 按官方 everything_ipc.h 解包，findx2-service
//! 兼容层（FindX v1 客户端布局）字段顺序不同。首查用官方布局发空串探测——
//! 官方语义空串命中全库（totitems>0），findx2 把 search 串误读为空回 0 命中——
//! 据此选定布局并缓存；查询失败时清缓存，服务端更换后自动重新协商。

use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
    ERROR_CLASS_ALREADY_EXISTS, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW,
    IsWindow, MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassW,
    SendMessageTimeoutW, SetWindowLongPtrW, TranslateMessage, GWLP_USERDATA, HWND_MESSAGE,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, SMTO_NORMAL, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_COPYDATA, WM_QUIT, WNDCLASSW,
};

use crate::{QueryError, QueryResults, ResultItem};

/// 查询消息（COPYDATASTRUCT.dwData）：EVERYTHING_IPC_COPYDATAQUERYW。
/// （真 Everything 与 findx2-service 都以此值路由到宽字符 QUERY 处理器）
const IPC_COPYDATAQUERYW: usize = 2;
/// 结果回投消息（查询包 reply_copydata_message 与回投 dwData）。
const IPC_COPYDATARESULTSMESSAGE: usize = 1;
/// 布局探测超时（正常回投 <10ms；给冷缓存留余量）。
const PROBE_TIMEOUT: Duration = Duration::from_millis(180);
/// 关键词打分：findx 空串恒 0，不能靠空查询区分布局。
const PROBE_KW_TIMEOUT: Duration = Duration::from_millis(250);
const PROBE_KEYWORD: &str = "windows";

/// 搜索串截断（对齐 WPF SearchFragmentCapacity）。
const SEARCH_MAX_CHARS: usize = 2048;
/// max_results 夹紧上界（对齐 WPF Math.Clamp(maxResults, 1, 10_000)）。
const MAX_RESULTS_CAP: u32 = 10_000;

/// EVERYTHING_IPC_LIST 头部字节数（7 × DWORD，pack(1)）。
const LIST_HEADER_BYTES: usize = 28;
/// EVERYTHING_IPC_ITEM 字节数（3 × DWORD）。
const ITEM_BYTES: usize = 12;
/// EVERYTHING_IPC_ITEM.flags
const IPC_FOLDER: u32 = 0x1;
const IPC_DRIVE: u32 = 0x2;

struct ReplyState {
    /// 窗口过程经原始指针写入；UnsafeCell 防止 release 认定「从未赋值」。
    data: std::cell::UnsafeCell<Option<Vec<u8>>>,
    /// 内核事件：窗口过程 SetEvent，等待侧 MsgWait，release 优化无法抹掉。
    event: HANDLE,
}

impl ReplyState {
    fn new() -> Result<Self, QueryError> {
        let event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|_| QueryError::ReplyWindowFailed)?;
        Ok(Self {
            data: std::cell::UnsafeCell::new(None),
            event,
        })
    }
    fn clear(&self) {
        unsafe {
            *self.data.get() = None;
            let _ = ResetEvent(self.event);
        }
    }
    fn take(&self) -> Option<Vec<u8>> {
        unsafe {
            let v = (*self.data.get()).take();
            let _ = ResetEvent(self.event);
            v
        }
    }
    fn is_filled(&self) -> bool {
        unsafe { (*self.data.get()).is_some() }
    }
    fn store(&self, bytes: Vec<u8>) {
        unsafe {
            *self.data.get() = Some(bytes);
            let _ = SetEvent(self.event);
        }
    }
    fn close(&self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

/// 服务端查询包布局。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QueryLayout {
    /// Everything 1.4 everything_ipc.h（全 DWORD，pack(1)）：
    /// reply_hwnd@0(4) reply_copydata_message@4(4) search_flags@8
    /// offset@12 max_results@16 search_string@20
    Official14,
    /// Everything 1.5+ / 部分 64 位头文件：HWND/ULONG_PTR 各 8 字节：
    /// reply_hwnd@0(8) reply_copydata_message@8(8) search_flags@16
    /// offset@20 max_results@24 search_string@28
    Official64,
    /// findx2-service 兼容层（FindX v1 客户端布局，全 32 位字段）：
    /// max_results@0 offset@4 reply_copydata_message@8 search_flags@12
    /// reply_hwnd@16 search_string@20（回执窗口取 WM_COPYDATA.wParam）
    Findx,
}

/// 0=未探测，1=Official14，2=Official64，3=Findx。
static LAYOUT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// 已选中的 IPC 窗口（与 LAYOUT 一起缓存；服务端重启后 IsWindow 失败再协商）。
static TARGET_HWND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn layout_from_tag(tag: u8) -> Option<QueryLayout> {
    match tag {
        1 => Some(QueryLayout::Official14),
        2 => Some(QueryLayout::Official64),
        3 => Some(QueryLayout::Findx),
        _ => None,
    }
}

fn layout_tag(layout: QueryLayout) -> u8 {
    match layout {
        QueryLayout::Official14 => 1,
        QueryLayout::Official64 => 2,
        QueryLayout::Findx => 3,
    }
}

pub(crate) fn debug_layout() -> &'static str {
    match LAYOUT.load(std::sync::atomic::Ordering::Acquire) {
        1 => "official14",
        2 => "official64",
        3 => "findx",
        _ => "unset",
    }
}

unsafe extern "system" fn reply_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_COPYDATA {
        let cds = &*(lparam.0 as *const COPYDATASTRUCT);
        if cds.dwData == IPC_COPYDATARESULTSMESSAGE {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ReplyState;
            if !state.is_null() {
                // COPYDATA 缓冲区仅在调用期间有效，必须复制
                let bytes = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
                (*state).store(bytes.to_vec());
                return LRESULT(1);
            }
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 0=未注册，1=成功，2=失败（不再重试）。
static CLASS_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

unsafe fn ensure_reply_class() -> Result<(), QueryError> {
    use std::sync::atomic::Ordering;
    match CLASS_STATE.load(Ordering::Acquire) {
        1 => return Ok(()),
        2 => return Err(QueryError::ReplyWindowFailed),
        _ => {}
    }
    let hinstance = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => {
            CLASS_STATE.store(2, Ordering::Release);
            return Err(QueryError::ReplyWindowFailed);
        }
    };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(reply_wnd_proc),
        hInstance: hinstance,
        lpszClassName: w!("ClipxEverythingIpcWnd"),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        // 并发注册时后到者收到 ERROR_CLASS_ALREADY_EXISTS，视为成功
        if GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            CLASS_STATE.store(2, Ordering::Release);
            return Err(QueryError::ReplyWindowFailed);
        }
    }
    CLASS_STATE.store(1, Ordering::Release);
    Ok(())
}

/// Everything 1.4 / findx2 用精确类名；1.5 Alpha 带实例后缀
/// （`EVERYTHING_TASKBAR_NOTIFICATION (1.5a)`）。优先精确匹配。
fn ipc_class_rank(name: &[u16]) -> u8 {
    const PREFIX: &[u16] = &[
        b'E' as u16, b'V' as u16, b'E' as u16, b'R' as u16, b'Y' as u16,
        b'T' as u16, b'H' as u16, b'I' as u16, b'N' as u16, b'G' as u16,
        b'_' as u16, b'T' as u16, b'A' as u16, b'S' as u16, b'K' as u16,
        b'B' as u16, b'A' as u16, b'R' as u16, b'_' as u16, b'N' as u16,
        b'O' as u16, b'T' as u16, b'I' as u16, b'F' as u16, b'I' as u16,
        b'C' as u16, b'A' as u16, b'T' as u16, b'I' as u16, b'O' as u16,
        b'N' as u16,
    ];
    if name == PREFIX {
        2
    } else if name.starts_with(PREFIX) {
        1
    } else {
        0
    }
}

/// 查找 Everything / findx IPC 窗口（类名 EVERYTHING_TASKBAR_NOTIFICATION[ 后缀]）。
///
/// 精确类名在前、带实例后缀的在后。可能同时存在 Everything 与 findx 兼容窗，
/// 调用方按「真实关键词命中条数」挑选，避免打到空壳窗口。
pub(crate) fn find_everything_hwnd() -> Option<HWND> {
    find_everything_hwnds().into_iter().next()
}

fn find_everything_hwnds() -> Vec<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW};

    struct Found {
        exact: Vec<usize>,
        prefix: Vec<usize>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = &mut *(lparam.0 as *mut Found);
        let mut buf = [0u16; 80];
        let n = GetClassNameW(hwnd, &mut buf);
        let name = std::slice::from_raw_parts(buf.as_ptr(), n.max(0) as usize);
        match ipc_class_rank(name) {
            2 => found.exact.push(hwnd.0 as usize),
            1 => found.prefix.push(hwnd.0 as usize),
            _ => {}
        }
        BOOL(1)
    }

    unsafe {
        let mut found = Found {
            exact: Vec::new(),
            prefix: Vec::new(),
        };
        let _ = EnumWindows(Some(cb), LPARAM(&mut found as *mut Found as isize));
        let mut out = Vec::new();
        for h in found.exact.into_iter().chain(found.prefix) {
            if h != 0 && !out.iter().any(|x| *x == h) {
                out.push(h);
            }
        }
        out.into_iter().map(|h| HWND(h as *mut _)).collect()
    }
}

/// 本会话无 IPC 窗口、但 Everything 服务管道在场时，拉起用户态 `-startup`
/// 托盘客户端（在本会话创建 IPC 窗口，索引仍走 session 0 服务）。只尝试一次。
fn try_wake_everything_client() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WOKE: AtomicBool = AtomicBool::new(false);
    if WOKE.swap(true, Ordering::SeqCst) {
        return;
    }
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\Everything\Everything.exe",
        r"C:\Program Files\Everything 1.5a\Everything.exe",
        r"C:\Program Files (x86)\Everything\Everything.exe",
    ];
    for path in CANDIDATES {
        if !std::path::Path::new(path).is_file() {
            continue;
        }
        let mut cmd = std::process::Command::new(path);
        cmd.arg("-startup")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            cmd.creation_flags(DETACHED_PROCESS);
        }
        if cmd.spawn().is_err() {
            continue;
        }
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if find_everything_hwnd().is_some() {
                return;
            }
        }
    }
}

/// 查询前确保本会话能看见 IPC 窗口（必要时唤醒用户态客户端）。
fn ensure_everything_hwnd() -> Option<HWND> {
    if let Some(h) = find_everything_hwnd() {
        return Some(h);
    }
    try_wake_everything_client();
    find_everything_hwnd()
}

/// 后台预热：用关键词协商窗口+布局，避免空串把 findx 锁死在错误布局。
pub(crate) fn warmup() {
    let _ = query(PROBE_KEYWORD, 8, PROBE_KW_TIMEOUT);
}

fn clear_target_cache() {
    use std::sync::atomic::Ordering;
    LAYOUT.store(0, Ordering::Release);
    TARGET_HWND.store(0, Ordering::Release);
}

fn store_target(hwnd: HWND, layout: QueryLayout) {
    use std::sync::atomic::Ordering;
    TARGET_HWND.store(hwnd.0 as usize, Ordering::Release);
    LAYOUT.store(layout_tag(layout), Ordering::Release);
}

fn cached_target() -> Option<(HWND, QueryLayout)> {
    use std::sync::atomic::Ordering;
    let h = TARGET_HWND.load(Ordering::Acquire);
    if h == 0 {
        return None;
    }
    let hwnd = HWND(h as *mut _);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            clear_target_cache();
            return None;
        }
    }
    layout_from_tag(LAYOUT.load(Ordering::Acquire)).map(|l| (hwnd, l))
}

/// 同步查询：构造查询包 → 投递 → 本线程泵消息等回投 → 解析。
pub(crate) fn query(
    search: &str,
    max_results: u32,
    timeout: Duration,
) -> Result<QueryResults, QueryError> {
    if ensure_everything_hwnd().is_none() {
        return Err(QueryError::NotRunning);
    }
    let max_results = max_results.clamp(1, MAX_RESULTS_CAP);
    let search: String = search.chars().take(SEARCH_MAX_CHARS).collect();

    unsafe {
        ensure_reply_class()?;

        let mut state = Box::new(ReplyState::new()?);
        let reply_hwnd = match create_reply_window(&mut *state) {
            Ok(h) => h,
            Err(e) => {
                state.close();
                return Err(e);
            }
        };

        let Some((everything, layout)) = pick_target(reply_hwnd, &mut state) else {
            let _ = DestroyWindow(reply_hwnd);
            state.close();
            return Err(QueryError::NotRunning);
        };
        state.clear();
        let parsed = pump_parse(
            everything,
            reply_hwnd,
            &search,
            max_results,
            timeout,
            &mut state,
            layout,
        );

        let _ = DestroyWindow(reply_hwnd);
        state.close();
        if parsed.is_err() {
            clear_target_cache();
        }
        parsed
    }
}

unsafe fn pump_parse(
    everything: HWND,
    reply_hwnd: HWND,
    search: &str,
    max_results: u32,
    timeout: Duration,
    state: &mut ReplyState,
    layout: QueryLayout,
) -> Result<QueryResults, QueryError> {
    state.clear();
    let result = send_and_pump(
        everything,
        reply_hwnd,
        search,
        max_results,
        timeout,
        state,
        layout,
    );
    if result.is_err() && state.is_filled() {
        let data = state.take().unwrap_or_default();
        return parse_results(&data).ok_or(QueryError::InvalidReply);
    }
    result?;
    let data = state.take().unwrap_or_default();
    parse_results(&data).ok_or(QueryError::InvalidReply)
}

unsafe fn pick_target(
    reply_hwnd: HWND,
    state: &mut ReplyState,
) -> Option<(HWND, QueryLayout)> {
    if let Some(cached) = cached_target() {
        return Some(cached);
    }
    let mut hwnds = find_everything_hwnds();
    if hwnds.is_empty() {
        try_wake_everything_client();
        hwnds = find_everything_hwnds();
    }
    if hwnds.is_empty() {
        return None;
    }

    // 1) Everything 官方空串命中全库。findx 空串恒 0。
    for hwnd in &hwnds {
        if probe_hits(*hwnd, reply_hwnd, state, QueryLayout::Official14, "", 1, PROBE_TIMEOUT) > 0
        {
            store_target(*hwnd, QueryLayout::Official14);
            return Some((*hwnd, QueryLayout::Official14));
        }
        if probe_hits(*hwnd, reply_hwnd, state, QueryLayout::Official64, "", 1, PROBE_TIMEOUT) > 0
        {
            store_target(*hwnd, QueryLayout::Official64);
            return Some((*hwnd, QueryLayout::Official64));
        }
    }

    // 2) 空串全 0：findx 兼容层。用空查询能回包（tot=0 也算成功）的窗口。
    for hwnd in &hwnds {
        if probe_ok(
            *hwnd,
            reply_hwnd,
            state,
            QueryLayout::Findx,
            "",
            1,
            PROBE_TIMEOUT,
        ) {
            store_target(*hwnd, QueryLayout::Findx);
            return Some((*hwnd, QueryLayout::Findx));
        }
    }
    let hwnd = hwnds[0];
    store_target(hwnd, QueryLayout::Findx);
    Some((hwnd, QueryLayout::Findx))
}

unsafe fn probe_ok(
    everything: HWND,
    reply_hwnd: HWND,
    state: &mut ReplyState,
    layout: QueryLayout,
    search: &str,
    max_results: u32,
    timeout: Duration,
) -> bool {
    state.clear();
    let ok = send_and_pump(
        everything,
        reply_hwnd,
        search,
        max_results,
        timeout,
        state,
        layout,
    )
    .is_ok();
    let _ = state.take();
    ok
}

unsafe fn probe_hits(
    everything: HWND,
    reply_hwnd: HWND,
    state: &mut ReplyState,
    layout: QueryLayout,
    search: &str,
    max_results: u32,
    timeout: Duration,
) -> u32 {
    state.clear();
    if send_and_pump(
        everything,
        reply_hwnd,
        search,
        max_results,
        timeout,
        state,
        layout,
    )
    .is_err()
    {
        return 0;
    }
    parse_results(&state.take().unwrap_or_default())
        .map(|r| r.total_items.max(r.items.len() as u32))
        .unwrap_or(0)
}

unsafe fn create_reply_window(state: *mut ReplyState) -> Result<HWND, QueryError> {
    let hinstance = GetModuleHandleW(None)
        .map(|h| h.into())
        .map_err(|_| QueryError::ReplyWindowFailed)?;
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("ClipxEverythingIpcWnd"),
        w!(""),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE), // message-only：不进 EnumWindows、不显示
        None,
        Some(hinstance),
        None,
    )
    .map_err(|_| QueryError::ReplyWindowFailed)?;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    Ok(hwnd)
}

/// Everything 1.4 QUERYW：五个 DWORD + UTF-16 搜索串。
fn build_packet_official14(reply_hwnd: HWND, search: &str, max_results: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(20 + (search.len() + 1) * 2);
    packet.extend_from_slice(&(reply_hwnd.0 as usize as u32).to_le_bytes());
    packet.extend_from_slice(&(IPC_COPYDATARESULTSMESSAGE as u32).to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes()); // search_flags：不设 MATCHPATH
    packet.extend_from_slice(&0u32.to_le_bytes()); // offset
    packet.extend_from_slice(&max_results.to_le_bytes());
    for u in search.encode_utf16().chain(std::iter::once(0)) {
        packet.extend_from_slice(&u.to_le_bytes());
    }
    packet
}

/// 官方 EVERYTHING_IPC_QUERYW（pack(1)，x64：HWND/ULONG_PTR 各 8 字节）：
/// reply_hwnd@0 | reply_copydata_message@8 | search_flags@16 | offset@20 |
/// max_results@24 | search_string@28（UTF-16 NUL 终止）
fn build_packet_official64(reply_hwnd: HWND, search: &str, max_results: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(28 + (search.len() + 1) * 2);
    packet.extend_from_slice(&(reply_hwnd.0 as usize).to_le_bytes());
    packet.extend_from_slice(&IPC_COPYDATARESULTSMESSAGE.to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes()); // search_flags：不设 MATCHPATH（WPF 注记）
    packet.extend_from_slice(&0u32.to_le_bytes()); // offset
    packet.extend_from_slice(&max_results.to_le_bytes());
    for u in search.encode_utf16().chain(std::iter::once(0)) {
        packet.extend_from_slice(&u.to_le_bytes());
    }
    packet
}

/// findx2-service 兼容层 QUERYW 布局（全 32 位字段，回执窗口取 wParam）：
/// max_results@0 | offset@4 | reply_copydata_message@8 | search_flags@12 |
/// reply_hwnd@16 | search_string@20
fn build_packet_findx(reply_hwnd: HWND, search: &str, max_results: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(20 + (search.len() + 1) * 2);
    packet.extend_from_slice(&max_results.to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes()); // offset
    packet.extend_from_slice(&(IPC_COPYDATARESULTSMESSAGE as u32).to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes()); // search_flags
    packet.extend_from_slice(&(reply_hwnd.0 as usize as u32).to_le_bytes());
    for u in search.encode_utf16().chain(std::iter::once(0)) {
        packet.extend_from_slice(&u.to_le_bytes());
    }
    packet
}

/// 投递查询并泵消息直到回投或超时。Ok = state.data 已填充。
#[allow(clippy::too_many_arguments)]
unsafe fn send_and_pump(
    everything: HWND,
    reply_hwnd: HWND,
    search: &str,
    max_results: u32,
    timeout: Duration,
    state: &mut ReplyState,
    layout: QueryLayout,
) -> Result<(), QueryError> {
    state.clear();
    let _ = ResetEvent(state.event);
    let packet = match layout {
        QueryLayout::Official14 => build_packet_official14(reply_hwnd, search, max_results),
        QueryLayout::Official64 => build_packet_official64(reply_hwnd, search, max_results),
        QueryLayout::Findx => build_packet_findx(reply_hwnd, search, max_results),
    };
    let cds = COPYDATASTRUCT {
        dwData: IPC_COPYDATAQUERYW,
        cbData: packet.len() as u32,
        lpData: packet.as_ptr() as *mut _,
    };
    let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    let deadline = Instant::now() + timeout;

    let mut send_result = 0usize;
    let sent = SendMessageTimeoutW(
        everything,
        WM_COPYDATA,
        WPARAM(reply_hwnd.0 as usize),
        LPARAM(&cds as *const COPYDATASTRUCT as isize),
        SMTO_NORMAL,
        timeout_ms,
        Some(&mut send_result),
    );
    // LTO 可能认定 packet/cds 在取指针后已死、提前复用栈槽；Everything
    // 在 SendMessage 期间仍读这块缓冲，必须钉到调用返回之后。
    std::hint::black_box(&packet);
    std::hint::black_box(&cds);

    if std::hint::black_box(state.is_filled()) {
        return Ok(());
    }
    if sent.0 == 0 {
        return Err(QueryError::SendFailed);
    }

    let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
    while !std::hint::black_box(state.is_filled()) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait_ms = (deadline - now).as_millis().clamp(1, u32::MAX as u128) as u32;
        let wr = MsgWaitForMultipleObjectsEx(
            Some(&[state.event]),
            wait_ms,
            QS_ALLINPUT,
            MWMO_INPUTAVAILABLE,
        );
        if wr == WAIT_OBJECT_0 {
            break;
        }
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if state.is_filled() {
        Ok(())
    } else {
        Err(QueryError::Timeout)
    }
}

/// 解析 EVERYTHING_IPC_LIST（pack(1)）：7×DWORD 头 + N×EVERYTHING_IPC_ITEM +
/// 宽字符串区。item 内 offset 为**字节**偏移（官方宏 `(WCHAR*)((CHAR*)list + off)`），
/// 相对列表头；findx2 兼容层回包同此约定。
fn parse_results(data: &[u8]) -> Option<QueryResults> {
    if data.len() < LIST_HEADER_BYTES {
        return None;
    }
    let u32_at = |o: usize| -> Option<u32> {
        data.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let total_folders = u32_at(0)?;
    let total_files = u32_at(4)?;
    let total_items = u32_at(8)?;
    let num_items = u32_at(20)?;

    let mut items = Vec::with_capacity(num_items as usize);
    for i in 0..num_items as usize {
        let base = LIST_HEADER_BYTES + i * ITEM_BYTES;
        let flags = u32_at(base)?;
        let filename_off = u32_at(base + 4)? as usize;
        let path_off = u32_at(base + 8)? as usize;
        let file_name = read_utf16z(data, filename_off)?;
        let path = read_utf16z(data, path_off)?;
        items.push(ResultItem {
            full_path: join_path(&path, &file_name),
            file_name,
            is_folder: flags & IPC_FOLDER != 0,
            is_drive: flags & IPC_DRIVE != 0,
        });
    }
    Some(QueryResults {
        total_items,
        total_folders,
        total_files,
        items,
    })
}

fn read_utf16z(data: &[u8], byte_off: usize) -> Option<String> {
    let mut units = Vec::new();
    let mut o = byte_off;
    loop {
        let b = data.get(o..o.checked_add(2)?)?;
        let u = u16::from_le_bytes([b[0], b[1]]);
        if u == 0 {
            break;
        }
        units.push(u);
        o += 2;
    }
    Some(String::from_utf16_lossy(&units))
}

fn join_path(path: &str, name: &str) -> String {
    if path.is_empty() {
        return name.to_string();
    }
    if name.is_empty() {
        return path.to_string();
    }
    if path.ends_with('\\') || path.ends_with('/') {
        format!("{path}{name}")
    } else {
        format!("{path}\\{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_le(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn official14_packet_layout() {
        let hwnd = HWND(0x1234 as *mut _);
        let p = build_packet_official14(hwnd, "ab", 7);
        assert_eq!(p.len(), 20 + 6);
        assert_eq!(u32_le(&p, 0), 0x1234); // reply_hwnd
        assert_eq!(u32_le(&p, 4), IPC_COPYDATARESULTSMESSAGE as u32);
        assert_eq!(u32_le(&p, 8), 0); // search_flags
        assert_eq!(u32_le(&p, 12), 0); // offset
        assert_eq!(u32_le(&p, 16), 7); // max_results
        assert_eq!(&p[20..], [b'a', 0, b'b', 0, 0, 0]);
    }

    #[test]
    fn official64_packet_layout() {
        let hwnd = HWND(0x1234 as *mut _);
        let p = build_packet_official64(hwnd, "ab", 7);
        // x64：hwnd 8B + msg 8B + flags/offset/max 各 4B + (2+1)*2B
        assert_eq!(p.len(), 28 + 6);
        assert_eq!(usize::from_le_bytes(p[0..8].try_into().unwrap()), 0x1234);
        assert_eq!(u32_le(&p, 8), IPC_COPYDATARESULTSMESSAGE as u32);
        assert_eq!(u32_le(&p, 12), 0); // msg 高 4 字节
        assert_eq!(u32_le(&p, 16), 0); // search_flags
        assert_eq!(u32_le(&p, 20), 0); // offset
        assert_eq!(u32_le(&p, 24), 7); // max_results
        let s = &p[28..];
        assert_eq!(s, [b'a', 0, b'b', 0, 0, 0]);
    }

    #[test]
    fn findx_packet_layout() {
        let hwnd = HWND(0x1234 as *mut _);
        let p = build_packet_findx(hwnd, "ab", 7);
        assert_eq!(p.len(), 20 + 6);
        assert_eq!(u32_le(&p, 0), 7); // max_results
        assert_eq!(u32_le(&p, 4), 0); // offset
        assert_eq!(u32_le(&p, 8), IPC_COPYDATARESULTSMESSAGE as u32);
        assert_eq!(u32_le(&p, 12), 0); // search_flags
        assert_eq!(u32_le(&p, 16), 0x1234); // reply_hwnd
        assert_eq!(&p[20..], [b'a', 0, b'b', 0, 0, 0]);
    }

    fn u16s(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(|u| u.to_le_bytes())
            .collect()
    }

    #[test]
    fn parse_folder_item_list() {
        let path_w = u16s("C:\\tools"); // 9 wchar（含终止）
        let name_w = u16s("clipx"); // 6 wchar
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes()); // totfolders
        buf.extend_from_slice(&0u32.to_le_bytes()); // totfiles
        buf.extend_from_slice(&1u32.to_le_bytes()); // totitems
        buf.extend_from_slice(&1u32.to_le_bytes()); // numfolders
        buf.extend_from_slice(&0u32.to_le_bytes()); // numfiles
        buf.extend_from_slice(&1u32.to_le_bytes()); // numitems
        buf.extend_from_slice(&0u32.to_le_bytes()); // offset
        // item 内 offset 为字节偏移（相对列表头）
        let path_off = LIST_HEADER_BYTES + ITEM_BYTES; // 40
        let name_off = path_off + path_w.len(); // 58
        buf.extend_from_slice(&IPC_FOLDER.to_le_bytes()); // item0 flags
        buf.extend_from_slice(&(name_off as u32).to_le_bytes());
        buf.extend_from_slice(&(path_off as u32).to_le_bytes());
        buf.extend_from_slice(&path_w);
        buf.extend_from_slice(&name_w);

        let r = parse_results(&buf).unwrap();
        assert_eq!(r.total_items, 1);
        assert_eq!(r.total_folders, 1);
        assert_eq!(r.total_files, 0);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].full_path, "C:\\tools\\clipx");
        assert_eq!(r.items[0].file_name, "clipx");
        assert!(r.items[0].is_folder);
        assert!(!r.items[0].is_drive);
    }

    #[test]
    fn parse_drive_item_and_malformed() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // totfolders
        buf.extend_from_slice(&1u32.to_le_bytes()); // totfiles
        buf.extend_from_slice(&1u32.to_le_bytes()); // totitems
        buf.extend_from_slice(&0u32.to_le_bytes()); // numfolders
        buf.extend_from_slice(&1u32.to_le_bytes()); // numfiles
        buf.extend_from_slice(&1u32.to_le_bytes()); // numitems
        buf.extend_from_slice(&0u32.to_le_bytes()); // offset
        // item0：盘符项，path 为空串（直接 0 终止），filename "C:"
        let str_base = LIST_HEADER_BYTES + ITEM_BYTES; // 40
        buf.extend_from_slice(&IPC_DRIVE.to_le_bytes());
        buf.extend_from_slice(&((str_base + 2) as u32).to_le_bytes()); // filename_off
        buf.extend_from_slice(&(str_base as u32).to_le_bytes()); // path_off → 空串
        buf.extend_from_slice(&[0, 0]); // path = ""
        buf.extend_from_slice(&u16s("C:"));

        let r = parse_results(&buf).unwrap();
        assert_eq!(r.items[0].full_path, "C:");
        assert!(r.items[0].is_drive);
        assert!(!r.items[0].is_folder);

        // 头部截断 → 非法
        assert!(parse_results(&buf[..20]).is_none());
        // 声称 2 条但只有 1 条数据 → 非法
        let mut bad = buf.clone();
        bad[20..24].copy_from_slice(&2u32.to_le_bytes());
        assert!(parse_results(&bad).is_none());
    }

    #[test]
    fn ipc_class_rank_exact_and_15a_suffix() {
        let exact: Vec<u16> = "EVERYTHING_TASKBAR_NOTIFICATION".encode_utf16().collect();
        let v15: Vec<u16> = "EVERYTHING_TASKBAR_NOTIFICATION (1.5a)"
            .encode_utf16()
            .collect();
        let other: Vec<u16> = "EVERYTHING".encode_utf16().collect();
        assert_eq!(ipc_class_rank(&exact), 2);
        assert_eq!(ipc_class_rank(&v15), 1);
        assert_eq!(ipc_class_rank(&other), 0);
    }

    #[test]
    fn live_roundtrip_if_everything_running() {
        match query("windows", 5, crate::DEFAULT_TIMEOUT) {
            Err(crate::QueryError::NotRunning) => {
                eprintln!("skip: Everything 未运行");
            }
            Ok(r) => {
                eprintln!(
                    "layout={} items={} tot={}",
                    crate::debug_layout(),
                    r.items.len(),
                    r.total_items
                );
                assert!(r.items.len() <= 5, "应被 max_results 截断");
                for it in &r.items {
                    assert!(!it.full_path.is_empty());
                }
            }
            Err(e) => panic!("live query: {e} layout={}", crate::debug_layout()),
        }
    }

    #[test]
    fn live_parent_scoped_query() {
        // C:\Windows 必被 Everything 索引；带关键词避免无 needle 时兼容层回空
        let search = crate::search::build_parent_scoped_search(r"C:\Windows", "system32");
        match query(&search, 10, crate::DEFAULT_TIMEOUT) {
            Err(crate::QueryError::NotRunning) => {
                eprintln!("skip: Everything 未运行");
            }
            Ok(r) => {
                assert!(!r.items.is_empty(), "parent:C:\\Windows system32 不应为空");
                assert!(
                    r.items.iter().any(|i| i.full_path
                        .to_ascii_lowercase()
                        .contains(r"c:\windows\system32")),
                    "应命中 System32：{:?}",
                    r.items.iter().map(|i| &i.full_path).collect::<Vec<_>>()
                );
            }
            Err(e) => panic!("live parent query: {e}"),
        }
    }
}
