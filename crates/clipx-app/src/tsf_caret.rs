//! TSF 光标信标的 clipx 侧消费者（Windows）。
//!
//! 职责：与 `crates/clipx-tsf` 的信标 DLL 通过共享内存 `Local\clipx-tsf-caret` 通信。
//!
//! 流程：
//! 1. 打开/创建共享内存视图。
//! 2. 控制块写 `request_seq`（自增），表示「请测一次当前光标」。
//! 3. 等 `ack_seq >= request_seq`（信标常驻线程每 40ms 轮询一次控制块）。
//! 4. 扫描结果槽 1..=63，取「前台 pid + 足够新鲜 + 尺寸合理」的最新一条。
//!
//! 协议常量与解析器在 `clipx-core::tsf_caret`（纯 Rust，三平台可测）。

use std::sync::atomic::{AtomicU32, Ordering};

use clipx_core::tsf_caret as proto;

/// 控制块魔数验证失败时的重试间隔（毫秒）。
const POLL_INTERVAL_MS: u64 = 8;
/// 等应答的总超时（毫秒）。
const ACK_TIMEOUT_MS: u64 = 400;
/// 结果槽的新鲜度上限（毫秒）。
pub const MAX_AGE_MS: u64 = 1500;

static REQUEST_SEQ: AtomicU32 = AtomicU32::new(1);

/// 共享内存视图（进程内单例）。
struct ShmView {
    ptr: *mut u8,
}

// 共享内存视图的指针是进程内固定的裸指针，跨线程只做定长读写。
unsafe impl Send for ShmView {}
unsafe impl Sync for ShmView {}

static SHM: std::sync::OnceLock<Option<ShmView>> = std::sync::OnceLock::new();

fn shm_view() -> Option<&'static ShmView> {
    SHM.get_or_init(|| {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
        };

        let name: Vec<u16> = proto::SHM_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let mapping = unsafe {
            CreateFileMappingW(
                HANDLE::default(),
                None,
                PAGE_READWRITE,
                0,
                (proto::SLOT_COUNT * proto::SLOT_SIZE) as u32,
                PCWSTR(name.as_ptr()),
            )
        };
        let Ok(mapping) = mapping else { return None };
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            return None;
        }
        Some(ShmView {
            ptr: view.Value as *mut u8,
        })
    })
    .as_ref()
}

unsafe fn read_u32(base: *mut u8, index: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base as *const u32).add(index)) }
}

unsafe fn write_u32(base: *mut u8, index: usize, value: u32) {
    unsafe { core::ptr::write_volatile((base as *mut u32).add(index), value) }
}

/// 共享内存视图指针（注入器与本模块共用）。
pub fn shm_init() -> Option<*mut u8> {
    shm_view().map(|v| v.ptr)
}

/// 写一次「请测一次」请求，返回本次 request_seq。
///
/// 首次使用会把控制块魔数/版本写好（信标侧只认 MAGIC_CTRL）。
///
/// **request_seq 必须跨 clipx 进程单调递增**：目标进程里的钩子用
/// `request_seq != ack_seq` 判断「有没有新请求」，而 `REQUEST_SEQ` 这个
/// 进程内计数器每次重启都从 1 开始，会与上次运行留下的 ack 撞车
/// （实测：第二次注入因 req==ack 被钩子直接跳过）。这里以共享内存里
/// 已有的值为基准取大者。
pub fn write_request(base: *mut u8) -> Option<u32> {
    let tick = now_tick();
    let seq = unsafe {
        let magic = read_u32(base, proto::ctrl::MAGIC);
        if magic != proto::MAGIC_CTRL {
            write_u32(base, proto::ctrl::MAGIC, proto::MAGIC_CTRL);
            write_u32(base, proto::ctrl::VERSION, proto::PROTOCOL_VERSION);
        }
        let prev = read_u32(base, proto::ctrl::REQUEST_SEQ);
        let local = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed);
        // 保证严格递增且不与历史 ack 冲突。
        let seq = prev.wrapping_add(1).max(local);
        write_u32(base, proto::ctrl::REQUEST_SEQ, seq);
        write_u32(base, proto::ctrl::REQUEST_TICK_LO, tick as u32);
        write_u32(base, proto::ctrl::REQUEST_TICK_HI, (tick >> 32) as u32);
        seq
    };
    Some(seq)
}

/// 扫结果槽，取目标 pid 的最新一条（不发请求、不等应答）。
///
/// `want_pid == 0` 表示不按 pid 过滤，只要最新一条合理的。
pub fn scan_results(base: *mut u8, want_pid: u32) -> Option<proto::CaretRect> {
    let mut slots: Vec<Option<proto::CaretRect>> = Vec::with_capacity(proto::SLOT_COUNT - 1);
    for i in 1..proto::SLOT_COUNT {
        let off = i * proto::SLOT_SIZE;
        let buf =
            unsafe { std::slice::from_raw_parts((base as *const u8).add(off), proto::SLOT_SIZE) };
        slots.push(proto::parse_slot(buf));
    }
    if want_pid == 0 {
        slots
            .iter()
            .flatten()
            .filter(|c| c.sane())
            .max_by_key(|c| (c.tick, c.seq))
            .copied()
    } else {
        proto::pick_latest(&slots, want_pid)
    }
}

/// 当前系统 tick（毫秒），与信标侧的 `GetTickCount64` 同源。
fn now_tick() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    unsafe { GetTickCount64() }
}

/// 请求信标测一次光标，并返回前台 pid 对应的最新结果。
///
/// `want_pid` 传 0 表示不按 pid 过滤（只要最新一条）。
pub fn request_caret(want_pid: u32) -> Option<proto::CaretRect> {
    let view = shm_view()?;
    let base = view.ptr;

    // 探测协议版本：v1（spike 旧 DLL）槽 0 即结果；v2 槽 0 是控制块。
    let magic0 = unsafe { read_u32(base, 0) };
    let version = match magic0 {
        proto::MAGIC_CTRL => 2u32,
        proto::MAGIC_SLOT => 1u32,
        // 首次使用：控制块还没初始化，按 v2 写好魔数/版本再走请求/应答。
        _ => 2u32,
    };

    if version == 1 {
        return read_v1_slot(base, want_pid);
    }

    let seq = write_request(base)?;

    // 等应答。
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ACK_TIMEOUT_MS);
    let mut acked = false;
    while std::time::Instant::now() < deadline {
        unsafe {
            if read_u32(base, proto::ctrl::ACK_SEQ) == seq {
                acked = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }
    if !acked {
        // 没等到应答也照样扫一遍结果槽 —— 信标可能在更早的请求里已经测过。
        log_once_per(3_000, &format!("beacon: ack timeout for seq={seq}"));
    }

    scan_results(base, want_pid).filter(|c| proto::is_fresh(c, now_tick(), MAX_AGE_MS))
}

/// v1 协议读取：槽 0 就是结果槽，没有请求/应答。
fn read_v1_slot(base: *mut u8, want_pid: u32) -> Option<proto::CaretRect> {
    let mut best: Option<proto::CaretRect> = None;
    for i in 0..proto::SLOT_COUNT {
        let off = i * proto::SLOT_SIZE;
        let buf = unsafe {
            std::slice::from_raw_parts((base as *const u8).add(off), proto::SLOT_SIZE)
        };
        let Some(c) = proto::parse_slot(buf) else { continue };
        if want_pid != 0 && c.pid != want_pid {
            continue;
        }
        if !c.sane() {
            continue;
        }
        best = match best {
            Some(b) if b.tick > c.tick || (b.tick == c.tick && b.seq >= c.seq) => Some(b),
            _ => Some(c),
        };
    }
    best
}

static LAST_LOG: std::sync::Mutex<Option<(std::time::Instant, String)>> =
    std::sync::Mutex::new(None);

/// 节流日志（同一类消息最多每 `interval_ms` 打一次）。
fn log_once_per(interval_ms: u64, msg: &str) {
    use std::time::Instant;
    let mut guard = LAST_LOG.lock().unwrap();
    let now = Instant::now();
    if let Some((t, _)) = guard.as_ref() {
        if (now.duration_since(*t).as_millis() as u64) < interval_ms {
            return;
        }
    }
    *guard = Some((now, msg.to_string()));
    let path = std::env::temp_dir().join("clipx_beacon_client.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", msg);
    }
}

/// 取当前前台窗口的进程 id。
pub fn foreground_pid() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let fg = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        pid
    }
}

/// 信标给出的矩形是**宿主客户区**坐标，需要换算到屏幕坐标。
///
/// 做法：拿前台窗口的 client→screen 偏移量加上。若结果明显不在前台窗口范围内，
/// 说明信标给的是屏幕坐标，直接用。
pub fn to_screen_point(rect: &proto::CaretRect) -> Option<(i32, i32)> {
    let (_l, t, r, b) = rect.rect;
    let (cx, cy) = rect.center();

    // 先按屏幕坐标试：落在前台窗口矩形内就认为是屏幕坐标。
    if let Some((wl, wt, wr, wb)) = foreground_window_rect() {
        if wl <= cx && cx <= wr && wt <= cy && cy <= wb {
            return Some((cx, cy));
        }
        // 否则按客户区偏移换算。
        if let Some((ox, oy)) = client_origin() {
            let sx = cx + ox;
            let sy = cy + oy;
            if wl <= sx && sx <= wr && wt <= sy && sy <= wb {
                return Some((sx, sy));
            }
        }
    }
    // 兜底：用矩形右边缘（光标竖线的位置）作锚点。
    Some((r, (t + b) / 2))
}

fn foreground_window_rect() -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let mut rc = RECT::default();
        GetWindowRect(fg, &mut rc).ok()?;
        Some((rc.left, rc.top, rc.right, rc.bottom))
    }
}

fn client_origin() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetForegroundWindow};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let mut rc = RECT::default();
        GetClientRect(fg, &mut rc).ok()?;
        let mut pt = POINT { x: rc.left, y: rc.top };
        // windows 0.59: ClientToScreen 返回 Result<Infallible>（不会失败）
        let _ = ClientToScreen(fg, &mut pt);
        Some((pt.x, pt.y))
    }
}

/// 对外主入口：拿当前前台窗口的光标屏幕坐标。
///
/// 返回 `(点, 来源标签)`，来源标签形如 `tsf:beacon`，便于写进 `pos_debug.log`。
///
/// 路线乙：先按需注入到前台进程测一次（不碰 profile / 不碰输入法）；
/// 注入拿不到再退回「读共享内存里已有结果」（兼容路线甲/TIP 常驻形态）。
pub fn caret_screen_point() -> Option<((i32, i32), String)> {
    let pid = foreground_pid();
    if pid == 0 {
        return None;
    }

    // 注入测量（按需、一次一个、用完即摘）。
    if let Some((rect, got_pid)) = crate::tsf_inject::measure_foreground_caret() {
        let _ = got_pid;
        let point = to_screen_point(&rect)?;
        return Some((point, "tsf:inject".to_string()));
    }

    // 兜底：读共享内存里的既有结果（路线甲常驻信标 / 协议 v1）。
    let rect = request_caret(pid)?;
    let point = to_screen_point(&rect)?;
    Some((point, "tsf:beacon".to_string()))
}
