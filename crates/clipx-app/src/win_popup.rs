use slint::Window;
use std::sync::atomic::{AtomicBool, Ordering};

/// WorkBuddy 等追不到真 caret 时：锚点是输入大框左上，禁止再翻到屏幕顶。
static PLACE_ON_BOX: AtomicBool = AtomicBool::new(false);
static INPUT_BOX: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);
static HOST_FG: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);

#[cfg(windows)]
use windows::Win32::Foundation::HWND;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE,
    WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

/// 必须在 `Window::show` 之后调用：winit 的 show 会重置 EXSTYLE，
/// 不跟 SetWindowPos(SWP_FRAMECHANGED) 的话 TOOLWINDOW/TOPMOST 不会生效，
/// 任务栏就会留下一个「Slint Window」，窗口本身却看不见。
#[cfg(windows)]
pub fn apply_style(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let add = (WS_EX_NOACTIVATE.0
            | WS_EX_TOOLWINDOW.0
            | WS_EX_TOPMOST.0
            | WS_EX_LAYERED.0) as isize;
        let remove = WS_EX_APPWINDOW.0 as isize;
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (current | add) & !remove);
        resize_hook::begin_our_pos();
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );
        resize_hook::end_our_pos();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

#[allow(dead_code)]
/// 0.4~1.0；依赖 `WS_EX_LAYERED`（`apply_style` 已加上）。
/// 弹窗透明度改走 Slint `panel-opacity`，避免 LWA_ALPHA 毁掉圆角外的 per-pixel 透明。
#[cfg(windows)]
pub fn apply_opacity(window: &Window, opacity: f64) {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::UI::WindowsAndMessaging::{SetLayeredWindowAttributes, LWA_ALPHA};
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let alpha = (opacity.clamp(0.4, 1.0) * 255.0).round() as u8;
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
    }
}

#[cfg(not(windows))]
pub fn apply_opacity(_window: &Window, _opacity: f64) {}

/// 按当前显示器 DPI 把逻辑尺寸写成物理像素。
/// 快查窗口启动时在主屏创建、弹出时挪到资源管理器屏幕：若仍按旧 scale 设 Logical，
/// 内容会按新 DPI 绘制而 HWND 还是旧大小，表现为放大后上方/右方被裁。
pub fn sync_logical_size(window: &Window, logical_w: f32, logical_h: f32) {
    let scale = window.scale_factor().max(1.0);
    window.set_size(slint::WindowSize::Physical(slint::PhysicalSize::new(
        (logical_w * scale).round() as u32,
        (logical_h * scale).round() as u32,
    )));
}

/// 弹窗矩形（纯函数，跨平台可测）。
///
/// 输入：物理锚点 `(ax, ay)`（已含偏移）、逻辑宽高、锚点屏缩放、锚点屏工作区。
/// 输出：`(x, y, 物理宽, 物理高)`。
/// 规则：右溢出 → 右贴边；下溢出 → **翻到光标正上方**（水平仍对齐插入点）。
/// 上方超出工作区时贴顶，不再侧移成一块「侧栏」——底栏输入要露出来。
pub fn popup_rect(
    ax: i32,
    ay: i32,
    logical_w: f32,
    logical_h: f32,
    scale_x: f64,
    scale_y: f64,
    work: (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let pw = ((logical_w as f64 * scale_x).round() as i32).max(1);
    let ph = ((logical_h as f64 * scale_y).round() as i32).max(1);
    let (wl, wt, wr, wb) = work;
    let mut x = ax;
    let mut y = ay;
    if x + pw > wr {
        x = wr - pw;
    }
    if PLACE_ON_BOX.load(Ordering::Relaxed) {
        if let Ok(guard) = INPUT_BOX.lock() {
            if let Some(box_rc) = *guard {
                let host = HOST_FG.lock().ok().and_then(|g| *g);
                return popup_rect_on_box(
                    box_rc, logical_w, logical_h, scale_x, scale_y, work, host,
                );
            }
        }
    }
    if y + ph > wb {
        y = ay - ph - 12;
    }
    if x < wl {
        x = wl;
    }
    if y < wt {
        y = wt;
    }
    if x + pw > wr {
        x = (wr - pw).max(wl);
    }
    if y + ph > wb {
        y = (wb - ph).max(wt);
    }
    (x, y, pw, ph)
}

/// 焦点控件是否是那块大输入（宽卡片），排除整窗和侧栏。
pub fn is_usable_composer(
    ctrl: Option<(f64, f64, f64, f64)>,
    fg: (i32, i32, i32, i32),
) -> bool {
    let Some((cl, _ct, cw, ch)) = ctrl else {
        return false;
    };
    let (l, t, r, b) = fg;
    let fw = (r - l) as f64;
    let fh = (b - t) as f64;
    if fw < 80.0 || fh < 80.0 {
        return false;
    }
    if cw < 240.0 || ch < 36.0 || ch > 280.0 || ch > fh * 0.45 {
        return false;
    }
    // 整页：又宽又高。底栏输入可以很宽（小窗里经常超过窗口 85%）。
    if cw > fw * 0.92 && ch > fh * 0.40 {
        return false;
    }
    // 窄侧栏控件，不要当成输入框。
    if cw < 280.0 && cl < (l as f64) + 72.0 {
        return false;
    }
    true
}

/// UIA 插入点（「今」）扩成输入卡片，弹窗贴这个框。
pub fn box_from_insert_pt(pt: (i32, i32), fg: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let (l, t, r, b) = fg;
    let il = pt.0.max(l + 80);
    let it = (pt.1 - 16).max(t + 24);
    let ir = (il + 760).min(r - 32);
    let ib = (it + 128).min(b - 12);
    (il, it, ir.max(il + 80), ib.max(it + 48))
}

/// 几何回退：对话/首页输入都在窗口底部，不要猜成窗中部卡片。
pub fn estimate_large_input_box(fg: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let (l, _t, r, b) = fg;
    let w = (r - l).max(1);
    let h = (b - fg.1).max(1);
    let pad_x = (w / 12).clamp(16, 48);
    let ih = (h * 16 / 100).clamp(72, 140);
    let il = l + pad_x;
    let ir = r - pad_x;
    let ib = b - 12;
    let it = ib - ih;
    (il, it, ir.max(il + 80), ib)
}

/// 相对前台窗判断上下：输入在窗下半（底栏）→ 放上面；在上半 → 放下面，否则贴地。
/// `host` 是前台窗；没有时才退回工作区中线（不要用屏幕中线判对话小窗）。
///
/// TODO(workbuddy-pos): 规则仍不稳，回头改。见 `resolve_popup_anchor` 同标记。
pub fn popup_rect_on_box(
    box_rc: (i32, i32, i32, i32),
    logical_w: f32,
    logical_h: f32,
    scale_x: f64,
    scale_y: f64,
    work: (i32, i32, i32, i32),
    host: Option<(i32, i32, i32, i32)>,
) -> (i32, i32, i32, i32) {
    let pw = ((logical_w as f64 * scale_x).round() as i32).max(1);
    let ph = ((logical_h as f64 * scale_y).round() as i32).max(1);
    let (wl, wt, wr, wb) = work;
    let (bl, bt, br, bb) = box_rc;
    let bw = (br - bl).max(1);
    let mut x = bl + (bw - pw) / 2;
    let lower = match host {
        Some((_, ft, _, fb)) => {
            let h = (fb - ft).max(1);
            bb >= ft + h * 65 / 100
        }
        None => bt >= wt + (wb - wt) / 2,
    };
    let mut y = if lower {
        let above = bt - ph - 12;
        if above >= wt {
            above
        } else {
            wt
        }
    } else {
        let below = bb + 12;
        if below + ph <= wb {
            below
        } else {
            (wb - ph).max(wt)
        }
    };
    if x < wl {
        x = wl;
    }
    if x + pw > wr {
        x = (wr - pw).max(wl);
    }
    if y < wt {
        y = wt;
    }
    if y + ph > wb {
        y = (wb - ph).max(wt);
    }
    (x, y, pw, ph)
}

/// 类名/进程名是否微信系（Weixin / WeChat / mmui 自绘输入）。
pub fn is_wechat_token(s: &str) -> bool {
    let n = s.to_ascii_lowercase();
    n.contains("weixin") || n.contains("wechat") || n.contains("mmui") || n.contains("chatinput")
}

/// 微信把客户区 (0,0) 当屏幕坐标时，锚点会落在主屏左上这块区域。
pub fn is_screen_origin_trap(x: i32, y: i32) -> bool {
    x.abs() < 96 && y.abs() < 96
}

/// 插入点是否能当锚点：必须在前台窗内，且不是桌面原点垃圾。
/// 不再用「窗口上半/下半」猜微信——大控件 BoundingRectangle 已不再当作光标。
pub fn caret_point_usable(
    x: i32,
    y: i32,
    fg: Option<(i32, i32, i32, i32)>,
    _wechat: bool,
) -> bool {
    if x.abs() < 8 && y.abs() < 8 {
        return false;
    }
    let Some(fg) = fg else {
        return true;
    };
    point_in_fg(x, y, fg)
}

/// UIA 选区是否像真实 caret。副屏允许负坐标；原点附近的退化矩形一律丢掉。
pub fn uia_text_sel_ok(
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    fg: Option<(i32, i32, i32, i32)>,
    wechat: bool,
) -> bool {
    if width < 0.0 || height <= 0.5 {
        return false;
    }
    if left.abs() < 2.0 && top.abs() < 2.0 {
        return false;
    }
    let x = left.round() as i32;
    let y = (top + height).round() as i32;
    caret_point_usable(x, y, fg, wechat)
}

/// Electron/Chromium 焦点控件 ClassName 常为空；窗口级矩形要拒，caret 大小的选区要收。
/// 零宽高条贴满输入框（高≈整块编辑区）也要拒，否则弹窗会盖住输入区。
pub fn uia_rect_is_caret_sized(width: f64, height: f64) -> bool {
    if width < 0.0 || width >= 120.0 || height <= 0.5 || height >= 80.0 {
        return false;
    }
    // 竖条撑满多行输入框：宽像 caret、高像控件。
    if width < 8.0 && height > 36.0 {
        return false;
    }
    true
}

/// 文本范围是否其实是整块输入框（相对焦点控件的 BoundingRectangle）。
pub fn uia_range_is_control_box(
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    ctrl: Option<(f64, f64, f64, f64)>,
) -> bool {
    let Some((cl, ct, cw, ch)) = ctrl else {
        return false;
    };
    if cw < 80.0 && ch < 40.0 {
        return false;
    }
    if (width - cw).abs() < 12.0 && (height - ch).abs() < 12.0 {
        return true;
    }
    if width > cw * 0.45 && height > ch * 0.45 {
        return true;
    }
    if width < 8.0 && ch > 40.0 && (height - ch).abs() < 12.0 {
        return true;
    }
    let on_left = (left - cl).abs() < 10.0;
    let on_bottom = ((top + height) - (ct + ch)).abs() < 10.0;
    if on_left && on_bottom && (cw > 80.0 || ch > 40.0) {
        return true;
    }
    false
}

/// 宽输入框左上角的「假 caret」：WorkBuddy 的选区/CaretRange 会钉在文档开头。
pub fn caret_at_pad_origin(x: i32, y: i32, ctrl: Option<(f64, f64, f64, f64)>) -> bool {
    let Some((cl, ct, cw, _ch)) = ctrl else {
        return false;
    };
    if cw < 180.0 {
        return false;
    }
    (x as f64 - cl) < 48.0 && (y as f64 - ct) < 64.0
}

/// 聊天/助手底栏：宽、不太高、落在窗口下半。WorkBuddy 首页焦点常在窗中部假 caret。
pub fn is_bottom_composer(
    ctrl: Option<(f64, f64, f64, f64)>,
    fg: Option<(i32, i32, i32, i32)>,
) -> bool {
    let Some((_cl, ct, cw, ch)) = ctrl else {
        return false;
    };
    if cw < 200.0 || ch < 36.0 {
        return false;
    }
    let Some((_, ft, _, fb)) = fg else {
        return false;
    };
    let h = (fb - ft) as f64;
    if h < 200.0 || ch > h * 0.55 {
        return false;
    }
    ct + ch > (ft as f64) + h * 0.55
}

/// 底栏圆角输入 + 底边距。锚到这个高度，弹窗翻上去后底边贴着输入框顶。
pub const BOTTOM_COMPOSER_RESERVE: i32 = 120;

/// 底栏输入控件顶边作为锚点（无 caret 时用）。
pub fn composer_box_anchor(ctrl: (f64, f64, f64, f64)) -> (i32, i32) {
    let (cl, ct, _cw, _ch) = ctrl;
    ((cl + 16.0).round() as i32, ct.round() as i32)
}

/// Electron 网页 caret 落在窗口上半（WorkBuddy 空 ClassName 假插入点）时，
/// 改锚到窗口底栏输入顶，让弹窗翻在输入框之上而不是贴工作区顶。
pub fn snap_mid_window_caret_to_bottom(
    pt: (i32, i32),
    fg: (i32, i32, i32, i32),
    web_ghost: bool,
) -> Option<(i32, i32)> {
    if !web_ghost {
        return None;
    }
    let (l, t, r, b) = fg;
    let h = b - t;
    if h < 400 {
        return None;
    }
    // 真插入点应在窗口最下 25%；中部假 caret（WorkBuddy 钉死在 ~y=476）要丢掉。
    if pt.1 >= t + h * 3 / 4 {
        return None;
    }
    let y = b - BOTTOM_COMPOSER_RESERVE;
    let x = pt.0.clamp(l + 16, r - 16);
    Some((x, y))
}

fn point_in_fg(x: i32, y: i32, fg: (i32, i32, i32, i32)) -> bool {
    let (l, t, r, b) = fg;
    // 半开 [left, right)：左副屏 rc.right=0 时，主屏原点 (0,y) 不算落在副屏上。
    x >= l && x < r && y >= t && y < b
}

/// 锚点所在显示器的工作区 + DPI（物理像素；失败回 96dpi + 虚拟大区，调用方照常夹紧）。
#[cfg(windows)]
fn monitor_work_and_dpi(x: i32, y: i32) -> ((i32, i32, i32, i32), (f64, f64)) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe {
        let pt = POINT { x, y };
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work = if GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            let w = mi.rcWork;
            (w.left, w.top, w.right, w.bottom)
        } else {
            (0, 0, 65535, 65535)
        };
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() {
            if dpi_x == 0 {
                dpi_x = 96;
            }
            if dpi_y == 0 {
                dpi_y = 96;
            }
        }
        (work, (dpi_x as f64 / 96.0, dpi_y as f64 / 96.0))
    }
}

/// 弹窗定位：光标所在显示器工作区，右下偏移 12px，越界回缩（WPF 版 M1 简化：鼠标锚点）。
#[cfg(windows)]
pub fn position_near_cursor(window: &Window, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return;
        }
        let ax = pt.x + 12;
        let ay = pt.y + 12;
        let (work, (sx, sy)) = monitor_work_and_dpi(ax, ay);
        let (x, y, pw, ph) = popup_rect(ax, ay, logical_w, logical_h, sx, sy, work);
        window.set_size(slint::WindowSize::Physical(slint::PhysicalSize::new(
            pw as u32,
            ph as u32,
        )));
        window.set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(x, y),
        ));
    }
}

/// 在逻辑线程解析锚点（物理像素，已含偏移）。UIA 在独立线程，超时不挡 UI。
/// 返回锚点 + 命中分支（错位现场抓取用，见 `append_pos_log`）。
/// 禁止回退到 (0,0)：那是主屏左上，GetCursorPos 失败或 UIA 原点噪声都会把弹窗甩过去。
#[cfg(windows)]
pub fn resolve_popup_anchor(mode: &str) -> (i32, i32, String) {
    PLACE_ON_BOX.store(false, Ordering::Relaxed);
    if let Ok(mut guard) = INPUT_BOX.lock() {
        *guard = None;
    }
    if let Ok(mut guard) = HOST_FG.lock() {
        *guard = None;
    }
    if mode == "Caret" {
        let exe = fg_exe_stem().to_ascii_lowercase();
        // TODO(workbuddy-pos) 2026-09-08：WorkBuddy 弹窗相对输入框仍经常偏，先日用、回头单开。
        // 现状：UIA 底栏经常 no-composer；几何回退猜窗底 + 相对前台窗 65% 判上下。
        // 对话小窗会盖聊天、首页偶贴地；不要再对齐 WPF。对照 Data/pos_debug.log 的 branch/rect。
        if exe.contains("workbuddy") {
            if let Some(fg) = fg_rect() {
                let (found, why) = wait_uia_composer_box(150, fg);
                let box_rc = found.unwrap_or_else(|| estimate_large_input_box(fg));
                if let Ok(mut guard) = INPUT_BOX.lock() {
                    *guard = Some(box_rc);
                }
                if let Ok(mut guard) = HOST_FG.lock() {
                    *guard = Some(fg);
                }
                PLACE_ON_BOX.store(true, Ordering::Relaxed);
                remember_anchor((box_rc.0, box_rc.1));
                return (box_rc.0, box_rc.1, format!("workbuddy:{why}"));
            }
        }
        let (pt, branch) = caret_screen_point_dbg();
        if let Some((x, y)) = pt {
            let fg = fg_rect();
            let exe = fg_exe_stem().to_ascii_lowercase();
            let web_ghost = branch.contains("cls=''") || exe.contains("workbuddy");
            let (x, y, branch) = match fg {
                Some(fg) => {
                    if let Some(s) = snap_mid_window_caret_to_bottom((x, y), fg, web_ghost) {
                        (s.0, s.1, format!("{branch}+bottom-snap"))
                    } else {
                        (x, y, branch)
                    }
                }
                None => (x, y, branch),
            };
            let a = (x, y + 8);
            if caret_point_usable(a.0, a.1, fg_rect(), is_wechat_fg()) {
                remember_anchor(a);
                return (a.0, a.1, format!("caret:{branch}"));
            }
            return recover_from_bad_caret(&format!("caret-trap({branch})"));
        }
        if let Some((x, y)) = cursor_screen_point() {
            let a = (x + 8, y + 20);
            remember_anchor(a);
            return (a.0, a.1, format!("caret-miss({branch})>cursor"));
        }
        return fallback_anchor(&format!("caret-miss({branch})"));
    }
    if let Some((x, y)) = cursor_screen_point() {
        let a = (x + 8, y + 20);
        remember_anchor(a);
        return (a.0, a.1, "cursor".to_string());
    }
    fallback_anchor("cursor-fail")
}

#[cfg(not(windows))]
pub fn resolve_popup_anchor(_mode: &str) -> (i32, i32, String) {
    (0, 0, "unsupported".to_string())
}

/// 定位诊断行：追加到 exe 旁 `Data/pos_debug.log`（>512KB 轮转一次）。
/// 呼出定位专用，不受 CLIPX_DEBUG 开关影响，方便对照错位分支。
#[cfg(windows)]
pub fn append_pos_log(line: &str) {
    write_data_log("pos_debug.log", line);
}

/// 通用诊断日志：exe 旁 `Data/{file}`（>512KB 轮转一次），失败一律忽略。
/// 须设环境变量 `CLIPX_DEBUG=1` 才写盘（钩子心跳默认关闭，避免日用刷盘）。
#[cfg(windows)]
pub fn append_debug_log(file: &str, line: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    let enabled = *ON.get_or_init(|| {
        std::env::var("CLIPX_DEBUG")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    });
    if !enabled {
        return;
    }
    write_data_log(file, line);
}

#[cfg(windows)]
fn write_data_log(file: &str, line: &str) {
    use std::io::Write as _;
    let mut path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    path.push("Data");
    let _ = std::fs::create_dir_all(&path);
    path.push(file);
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 512 * 1024 {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {line}");
    }
}

#[cfg(not(windows))]
pub fn append_debug_log(_file: &str, _line: &str) {}

#[cfg(not(windows))]
pub fn fg_debug() -> String {
    String::new()
}

/// 前台窗快照：hwnd/类名/pid/进程名/矩形（错位时判断 UIA 是不是看错了窗）。
#[cfg(windows)]
pub fn fg_debug() -> String {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
    };
    unsafe {
        let fg = GetForegroundWindow();
        let hwnd = fg.0 as isize;
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        let mut cls = [0u16; 256];
        let n = GetClassNameW(fg, &mut cls);
        let class = String::from_utf16_lossy(&cls[..(n as usize).min(cls.len())]);
        let mut exe = format!("pid:{pid}");
        if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut buf = [0u16; 260];
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .is_ok()
            {
                let full = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
                exe = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
            }
            let _ = windows::Win32::Foundation::CloseHandle(h);
        }
        let mut rc = windows::Win32::Foundation::RECT::default();
        let rect = if GetWindowRect(fg, &mut rc).is_ok() {
            format!("({},{},{},{})", rc.left, rc.top, rc.right, rc.bottom)
        } else {
            "?".to_string()
        };
        format!("fg=0x{hwnd:X} cls={class} exe={exe} fgrect={rect}")
    }
}

/// 不碰窗口、纯按锚点算出的工作区/DPI/终矩形（与 `place_in_work` 同源，逻辑线程可记）。
#[cfg(windows)]
pub fn placement_debug(logical_w: f32, logical_h: f32, x: i32, y: i32) -> String {
    let (work, (sx, sy)) = monitor_work_and_dpi(x, y);
    let (px, py, pw, ph) = popup_rect(x, y, logical_w, logical_h, sx, sy, work);
    format!(
        "anchor=({x},{y}) work=({},{},{},{}) dpi={sx:.2}/{sy:.2} rect=({px},{py},{pw}x{ph})",
        work.0, work.1, work.2, work.3
    )
}

/// 窗口当前 scale（show 前后对比：混 DPI 下首秀前是主屏的）。
#[cfg(windows)]
pub fn scale_now(window: &Window) -> f32 {
    window.scale_factor()
}

/// 把已解析的物理锚点放到所在屏工作区内。
/// show 前后各调一次（对齐 WPF `ShowPopup` 双定位）：show 前定好初始位置，
/// 首帧即正确、不闪主屏左上；show 后 winit 可能重置样式/尺寸，再确认一次。
#[cfg(windows)]
pub fn position_at(window: &Window, logical_w: f32, logical_h: f32, x: i32, y: i32) {
    place_in_work(window, logical_w, logical_h, x, y);
}

#[cfg(not(windows))]
pub fn position_at(window: &Window, logical_w: f32, logical_h: f32, _x: i32, _y: i32) {
    position_near_cursor(window, logical_w, logical_h);
}

/// `Caret`：UIA → Win32 caret → 缓存 → 鼠标（对齐 WPF PositionPopup）。
#[cfg(windows)]
pub fn position_popup(window: &Window, logical_w: f32, logical_h: f32, mode: &str) {
    let (x, y, _) = resolve_popup_anchor(mode);
    position_at(window, logical_w, logical_h, x, y);
}

#[cfg(not(windows))]
pub fn position_popup(window: &Window, logical_w: f32, logical_h: f32, _mode: &str) {
    position_near_cursor(window, logical_w, logical_h);
}

/// 启动时预热 UIA，避免第一次从 Word/Chromium 呼出时冷启动超时落到鼠标。
pub fn warmup_caret_uia() {
    #[cfg(windows)]
    {
        let _ = std::thread::Builder::new()
            .name("clipx-uia-warmup".into())
            .spawn(|| {
                let _ = uia_caret_point();
            });
    }
}

#[cfg(windows)]
fn cursor_screen_point() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        let p = (pt.x, pt.y);
        remember_cursor(p);
        Some(p)
    }
}

#[cfg(windows)]
static LAST_CURSOR: std::sync::Mutex<Option<(i32, i32)>> = std::sync::Mutex::new(None);
#[cfg(windows)]
static LAST_ANCHOR: std::sync::Mutex<Option<(i32, i32)>> = std::sync::Mutex::new(None);

#[cfg(windows)]
fn remember_cursor(pt: (i32, i32)) {
    if let Ok(mut g) = LAST_CURSOR.lock() {
        *g = Some(pt);
    }
}

#[cfg(windows)]
fn remember_anchor(pt: (i32, i32)) {
    if let Ok(mut g) = LAST_ANCHOR.lock() {
        *g = Some(pt);
    }
}

/// GetCursorPos 失败时：上次鼠标 → 前台窗内部 → 上次成功锚点 → 当前屏工作区中心。
/// 绝不返回 (0,0)（主屏左上）。
#[cfg(windows)]
fn fallback_anchor(why: &str) -> (i32, i32, String) {
    if let Some((x, y)) = LAST_CURSOR.lock().ok().and_then(|g| *g) {
        return (x + 8, y + 20, format!("{why}>last-cursor"));
    }
    if let Some((x, y)) = fg_window_anchor() {
        return (x, y, format!("{why}>fg"));
    }
    if let Some((x, y)) = LAST_ANCHOR.lock().ok().and_then(|g| *g) {
        return (x, y, format!("{why}>last-anchor"));
    }
    let (work, _) = monitor_work_and_dpi(1, 1);
    let cx = work.0 + (work.2 - work.0).max(0) / 2;
    let cy = work.1 + (work.3 - work.1).max(0) / 2;
    (cx, cy, format!("{why}>work-center"))
}

#[cfg(windows)]
fn recover_from_bad_caret(why: &str) -> (i32, i32, String) {
    let fg = fg_rect();
    if let Some((x, y)) = cursor_screen_point() {
        if fg.map(|f| point_in_fg(x, y, f)).unwrap_or(false) {
            let a = (x + 8, y + 20);
            remember_anchor(a);
            return (a.0, a.1, format!("{why}>cursor-in-fg"));
        }
    }
    if let Some((x, y)) = cursor_screen_point() {
        let a = (x + 8, y + 20);
        remember_anchor(a);
        return (a.0, a.1, format!("{why}>cursor"));
    }
    fallback_anchor(why)
}

#[cfg(windows)]
fn fg_window_anchor() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let mut rc = RECT::default();
        GetWindowRect(fg, &mut rc).ok()?;
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        if w <= 0 || h <= 0 {
            return None;
        }
        // 大窗锚在下部（微信输入区），不要 left+24/top+48 那就是标题栏左上。
        let x = rc.left + w / 3;
        let y = if h > 240 {
            rc.top + h * 4 / 5
        } else {
            rc.top + h / 2
        };
        Some((x, y))
    }
}

#[cfg(windows)]
fn fg_rect() -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let mut rc = RECT::default();
        GetWindowRect(fg, &mut rc).ok()?;
        if rc.right <= rc.left || rc.bottom <= rc.top {
            return None;
        }
        Some((rc.left, rc.top, rc.right, rc.bottom))
    }
}

#[cfg(windows)]
fn fg_exe_stem() -> String {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let fg = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let name = if QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok()
        {
            let full = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
            full.rsplit(['\\', '/'])
                .next()
                .unwrap_or(&full)
                .to_string()
        } else {
            String::new()
        };
        let _ = windows::Win32::Foundation::CloseHandle(h);
        name
    }
}

#[cfg(windows)]
fn is_wechat_fg() -> bool {
    is_wechat_token(&fg_exe_stem())
}

/// Chromium / Electron 宿主窗（WorkBuddy、VS Code、网页壳）。
pub fn is_chromium_token(s: &str) -> bool {
    let n = s.to_ascii_lowercase();
    n.contains("chrome_widgetwin") || n.contains("chrome_renderwidget")
}

#[cfg(windows)]
fn hwnd_class_name(hwnd: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    unsafe {
        let mut buf = [0u16; 128];
        let n = GetClassNameW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())])
    }
}

#[cfg(windows)]
fn is_chromium_fg() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { is_chromium_token(&hwnd_class_name(GetForegroundWindow())) }
}

#[cfg(windows)]
unsafe extern "system" fn push_render_widget(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<windows::Win32::Foundation::HWND>) };
    if hwnd_class_name(hwnd).contains("Chrome_RenderWidgetHostHWND") {
        out.push(hwnd);
    }
    windows::Win32::Foundation::BOOL(1)
}

#[cfg(windows)]
fn chromium_render_hwnds() -> Vec<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetForegroundWindow};
    unsafe {
        let fg = GetForegroundWindow();
        let mut hwnds: Vec<HWND> = Vec::new();
        if !fg.0.is_null() {
            hwnds.push(fg);
            let _ = EnumChildWindows(
                Some(fg),
                Some(push_render_widget),
                LPARAM(&mut hwnds as *mut Vec<HWND> as isize),
            );
        }
        hwnds
    }
}

#[cfg(windows)]
struct CaretCache {
    hwnd: isize,
    x: i32,
    y: i32,
    at: std::time::Instant,
}

#[cfg(windows)]
static CARET_CACHE: std::sync::Mutex<Option<CaretCache>> = std::sync::Mutex::new(None);

#[cfg(windows)]
fn cache_caret(hwnd: isize, pt: (i32, i32)) {
    if let Ok(mut g) = CARET_CACHE.lock() {
        *g = Some(CaretCache {
            hwnd,
            x: pt.0,
            y: pt.1,
            at: std::time::Instant::now(),
        });
    }
}

#[cfg(windows)]
fn cached_caret(hwnd: isize) -> Option<(i32, i32)> {
    let g = CARET_CACHE.lock().ok()?;
    let c = g.as_ref()?;
    if c.hwnd != hwnd || c.at.elapsed() > std::time::Duration::from_secs(30) {
        return None;
    }
    Some((c.x, c.y))
}

/// MSAA `OBJID_CARET`：Chromium 把插入符暴露成独立 accessible，不依赖 IMM。
#[cfg(windows)]
fn msaa_caret_on(hwnd: windows::Win32::Foundation::HWND) -> Option<(i32, i32)> {
    use windows::core::{Interface, Type};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
    use windows::Win32::UI::WindowsAndMessaging::OBJID_CARET;
    unsafe {
        if hwnd.0.is_null() {
            return None;
        }
        let mut raw: *mut core::ffi::c_void = core::ptr::null_mut();
        AccessibleObjectFromWindow(
            hwnd,
            OBJID_CARET.0 as u32,
            &IAccessible::IID,
            &mut raw,
        )
        .ok()?;
        if raw.is_null() {
            return None;
        }
        let acc: IAccessible = Type::from_abi(raw).ok()?;
        let child = VARIANT::from(0i32);
        if let Ok(role) = acc.get_accRole(&child) {
            if let Ok(v) = i32::try_from(&role) {
                if v != windows::Win32::UI::Accessibility::ROLE_SYSTEM_CARET as i32 {
                    return None;
                }
            }
        }
        let mut x = 0i32;
        let mut y = 0i32;
        let mut w = 0i32;
        let mut h = 0i32;
        acc.accLocation(&mut x, &mut y, &mut w, &mut h, &child)
            .ok()?;
        if !uia_rect_is_caret_sized(w as f64, h as f64) {
            return None;
        }
        let pt = (x, y + h);
        if pt.0 == 0 && pt.1 == 0 {
            return None;
        }
        Some(pt)
    }
}

#[cfg(windows)]
fn msaa_caret_point() -> Option<(i32, i32)> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };
    unsafe {
        let fg = GetForegroundWindow();
        let mut hwnds = chromium_render_hwnds();
        let tid = GetWindowThreadProcessId(fg, None);
        if tid != 0 {
            let mut info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            if GetGUIThreadInfo(tid, &mut info).is_ok() && !info.hwndCaret.0.is_null() {
                hwnds.insert(0, info.hwndCaret);
            }
        }
        for hwnd in hwnds {
            if let Some(pt) = msaa_caret_on(hwnd) {
                return Some(pt);
            }
        }
        None
    }
}

/// `IMR_QUERYCHARPOSITION`：IME 跟光标用的官方查询，Electron 在渲染窗上会答。
#[cfg(windows)]
fn ime_query_char_point() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::Input::Ime::{IMECHARPOSITION, IMR_QUERYCHARPOSITION};
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, WM_IME_REQUEST, SMTO_ABORTIFHUNG,
    };
    let fg_rc = fg_rect();
    unsafe {
        for hwnd in chromium_render_hwnds() {
            let mut icp = IMECHARPOSITION {
                dwSize: std::mem::size_of::<IMECHARPOSITION>() as u32,
                dwCharPos: 0,
                ..Default::default()
            };
            let mut result = 0usize;
            let lr = SendMessageTimeoutW(
                hwnd,
                WM_IME_REQUEST,
                WPARAM(IMR_QUERYCHARPOSITION as usize),
                LPARAM((&mut icp as *mut IMECHARPOSITION) as isize),
                SMTO_ABORTIFHUNG,
                50,
                Some(&mut result),
            );
            if lr.0 == 0 || result == 0 {
                continue;
            }
            let h = icp.cLineHeight as i32;
            // 行高接近整块输入框 / 文档时，这是控件矩形不是插入点。
            let doc_h = icp.rcDocument.bottom - icp.rcDocument.top;
            if h <= 0 || h > 36 || (doc_h > 40 && h >= doc_h - 8) {
                continue;
            }
            let mut pt = icp.pt;
            let screen = if fg_rc
                .map(|rc| point_in_fg(pt.x, pt.y, rc) || point_in_fg(pt.x, pt.y + h, rc))
                .unwrap_or(false)
            {
                (pt.x, pt.y + h)
            } else if ClientToScreen(hwnd, &mut pt).as_bool() {
                (pt.x, pt.y + h)
            } else {
                continue;
            };
            if uia_range_is_control_box(
                screen.0 as f64,
                (screen.1 - h) as f64,
                1.0,
                h as f64,
                fg_rc.map(|(l, t, r, b)| (l as f64, t as f64, (r - l) as f64, (b - t) as f64)),
            ) {
                continue;
            }
            if screen.0 != 0 || screen.1 != 0 {
                return Some(screen);
            }
        }
        None
    }
}

/// Electron 的系统 caret 挂在 `Chrome_RenderWidgetHostHWND` 上，不一定在顶层窗。
/// `GetCaretPos` 是线程级的，必须对拥有 caret 的那个子窗做 ClientToScreen。
#[cfg(windows)]
fn chromium_host_caret() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCaretPos, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
    };
    if !is_chromium_fg() {
        return None;
    }
    unsafe {
        let fg = GetForegroundWindow();
        let fg_tid = GetWindowThreadProcessId(fg, None);
        if fg_tid == 0 {
            return None;
        }
        let my_tid = GetCurrentThreadId();
        if !AttachThreadInput(my_tid, fg_tid, true).as_bool() {
            return None;
        }
        let mut caret = POINT::default();
        let ok = GetCaretPos(&mut caret).is_ok() && (caret.x != 0 || caret.y != 0);
        let mut found = None;
        if ok {
            for hwnd in chromium_render_hwnds() {
                if hwnd == fg {
                    continue;
                }
                let mut pt = caret;
                if !ClientToScreen(hwnd, &mut pt).as_bool() {
                    continue;
                }
                let mut rc = RECT::default();
                if GetWindowRect(hwnd, &mut rc).is_err() {
                    continue;
                }
                if pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom {
                    found = Some((pt.x, pt.y));
                    break;
                }
            }
        }
        let _ = AttachThreadInput(my_tid, fg_tid, false);
        found
    }
}

#[cfg(windows)]
fn wait_uia_caret(ms: u64) -> (Option<(i32, i32)>, String) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let _ = std::thread::Builder::new()
        .name("clipx-uia-caret".into())
        .spawn(move || {
            let (pt, branch) = uia_caret_point();
            let _ = tx.send((pt, branch));
        });
    rx.recv_timeout(std::time::Duration::from_millis(ms))
        .unwrap_or((None, "timeout".to_string()))
}

/// 只问焦点控件矩形 / 选区起点，不扫整棵 UIA 树（避免卡顿）。
#[cfg(windows)]
fn wait_uia_composer_box(ms: u64, fg: (i32, i32, i32, i32)) -> (Option<(i32, i32, i32, i32)>, String) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let _ = std::thread::Builder::new()
        .name("clipx-uia-box".into())
        .spawn(move || {
            let _ = tx.send(uia_composer_box(fg));
        });
    rx.recv_timeout(std::time::Duration::from_millis(ms))
        .unwrap_or((None, "timeout".to_string()))
}

#[cfg(windows)]
fn uia_composer_box(fg: (i32, i32, i32, i32)) -> (Option<(i32, i32, i32, i32)>, String) {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayUnaccessData};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
        TreeScope_Descendants, UIA_HasKeyboardFocusPropertyId, UIA_TextPatternId,
    };

    fn box_of(ctrl: (f64, f64, f64, f64)) -> (i32, i32, i32, i32) {
        let (cl, ct, cw, ch) = ctrl;
        (
            cl.round() as i32,
            ct.round() as i32,
            (cl + cw).round() as i32,
            (ct + ch).round() as i32,
        )
    }

    unsafe {
        let co = CoInitializeEx(None, COINIT_MULTITHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(v) => v,
                Err(_) => {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    return (None, "no-uia".to_string());
                }
            };
        let done = |v, why: String| {
            if co.is_ok() {
                CoUninitialize();
            }
            (v, why)
        };
        let mut el = match auto.GetFocusedElement() {
            Ok(e) => e,
            Err(_) => return done(None, "no-focus".to_string()),
        };
        let class = el
            .CurrentClassName()
            .ok()
            .map(|b| b.to_string())
            .unwrap_or_default();
        if class.contains("Chrome_WidgetWin") || class.contains("Chrome_RenderWidgetHostHWND") {
            if let Ok(cond) =
                auto.CreatePropertyCondition(UIA_HasKeyboardFocusPropertyId, &VARIANT::from(true))
            {
                if let Ok(child) = el.FindFirst(TreeScope_Descendants, &cond) {
                    el = child;
                }
            }
        }
        let bounds = |e: &IUIAutomationElement| {
            e.CurrentBoundingRectangle().ok().map(|r| {
                (
                    r.left as f64,
                    r.top as f64,
                    (r.right - r.left) as f64,
                    (r.bottom - r.top) as f64,
                )
            })
        };
        let mut best = None;
        let mut cur = el.clone();
        if let Ok(walker) = auto.ControlViewWalker() {
            for _ in 0..8 {
                let ctrl = bounds(&cur);
                if is_usable_composer(ctrl, fg) {
                    best = ctrl;
                }
                match walker.GetParentElement(&cur) {
                    Ok(p) => cur = p,
                    Err(_) => break,
                }
            }
        } else if is_usable_composer(bounds(&el), fg) {
            best = bounds(&el);
        }
        if let Some(ctrl) = best {
            let b = box_of(ctrl);
            return done(
                Some(b),
                format!(
                    "parent ({},{},{}x{})",
                    b.0,
                    b.1,
                    b.2 - b.0,
                    b.3 - b.1
                ),
            );
        }
        if let Ok(pattern) = el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
            if let Ok(sel) = pattern.GetSelection() {
                if sel.Length().unwrap_or(0) > 0 {
                    if let Ok(range) = sel.GetElement(0) {
                        if let Ok(psa) = range.GetBoundingRectangles() {
                            if !psa.is_null() {
                                let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
                                if SafeArrayAccessData(psa, &mut data).is_ok() && !data.is_null()
                                {
                                    let nums = data as *const f64;
                                    let left = *nums;
                                    let top = *nums.add(1);
                                    let _ = SafeArrayUnaccessData(psa);
                                    let _ = SafeArrayDestroy(psa);
                                    let pt = (left.round() as i32, top.round() as i32);
                                    if point_in_fg(pt.0, pt.1, fg) {
                                        return done(
                                            Some(box_from_insert_pt(pt, fg)),
                                            "focused-caret".to_string(),
                                        );
                                    }
                                } else {
                                    let _ = SafeArrayDestroy(psa);
                                }
                            }
                        }
                    }
                }
            }
        }
        done(None, "no-composer".to_string())
    }
}

#[cfg(windows)]
fn caret_screen_point_dbg() -> (Option<(i32, i32)>, String) {
    let fg = foreground_hwnd();
    let wechat = is_wechat_fg();
    let chromium = is_chromium_fg();
    let fg_rc = fg_rect();
    // 微信/Qt：IMM 是真插入点。Electron 的 IMM/IME 常填整块输入框，必须后置且加尺寸过滤。
    if !chromium {
        if let Some(p) = imm_caret_point() {
            if caret_point_usable(p.0, p.1, fg_rc, wechat) {
                cache_caret(fg, p);
                return (Some(p), "imm".to_string());
            }
        }
    }
    if let Some(p) = gui_thread_caret() {
        if caret_point_usable(p.0, p.1, fg_rc, wechat) {
            cache_caret(fg, p);
            return (Some(p), "gui".to_string());
        }
    }
    if let Some(p) = attached_caret() {
        if caret_point_usable(p.0, p.1, fg_rc, wechat) {
            cache_caret(fg, p);
            return (Some(p), "attached".to_string());
        }
    }
    // Electron：先 UIA（HasKeyboardFocus + 真 caret），避免 MSAA/IME 把输入框当光标。
    let mut uia_sub = "skip".to_string();
    if chromium {
        let (uia, sub) = wait_uia_caret(250);
        uia_sub = sub;
        if let Some(p) = uia {
            if caret_point_usable(p.0, p.1, fg_rc, wechat) {
                cache_caret(fg, p);
                return (Some(p), format!("uia:{uia_sub}"));
            }
        }
    }
    if let Some(p) = msaa_caret_point() {
        if caret_point_usable(p.0, p.1, fg_rc, wechat) {
            cache_caret(fg, p);
            return (Some(p), "msaa".to_string());
        }
    }
    if let Some(p) = ime_query_char_point() {
        if caret_point_usable(p.0, p.1, fg_rc, wechat) {
            cache_caret(fg, p);
            return (Some(p), "ime-char".to_string());
        }
    }
    if let Some(p) = chromium_host_caret() {
        if caret_point_usable(p.0, p.1, fg_rc, wechat) {
            cache_caret(fg, p);
            return (Some(p), "chrome-host".to_string());
        }
    }
    if !chromium {
        let (uia, sub) = wait_uia_caret(400);
        uia_sub = sub;
        if let Some(p) = uia {
            if caret_point_usable(p.0, p.1, fg_rc, wechat) {
                cache_caret(fg, p);
                return (Some(p), format!("uia:{uia_sub}"));
            }
        }
    }
    if let Some(p) = cached_caret(fg) {
        if caret_point_usable(p.0, p.1, fg_rc, wechat) {
            return (Some(p), format!("cached(uia:{uia_sub})"));
        }
    }
    (None, format!("miss(imm/gui/msaa/uia:{uia_sub})"))
}

#[cfg(windows)]
unsafe fn point_from_text_range(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
    class_name: &str,
    fg: Option<(i32, i32, i32, i32)>,
    wechat: bool,
    ctrl: Option<(f64, f64, f64, f64)>,
    at_end: bool,
) -> Option<((i32, i32), String)> {
    use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayUnaccessData};
    if let Ok(text) = range.GetText(24) {
        if text.to_string().chars().count() > 12 {
            return None;
        }
    }
    let psa = range.GetBoundingRectangles().ok()?;
    if psa.is_null() {
        return None;
    }
    let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
    if SafeArrayAccessData(psa, &mut data).is_err() || data.is_null() {
        let _ = SafeArrayDestroy(psa);
        return None;
    }
    let nums = data as *const f64;
    let left = *nums;
    let top = *nums.add(1);
    let width = *nums.add(2);
    let height = *nums.add(3);
    let _ = SafeArrayUnaccessData(psa);
    let _ = SafeArrayDestroy(psa);
    let seen = format!("({left:.0},{top:.0},{width:.0}x{height:.0}) cls='{class_name}'");
    if uia_range_is_control_box(left, top, width, height, ctrl) {
        return None;
    }
    if !uia_rect_is_caret_sized(width, height) {
        return None;
    }
    if uia_text_sel_ok(left, top, width, height, fg, wechat) {
        let x = if at_end {
            (left + width).round() as i32
        } else {
            left.round() as i32
        };
        Some(((x, (top + height).round() as i32), seen))
    } else {
        None
    }
}

/// UIA TextPattern 选区。空 ClassName 或铺满前台窗的矩形视为无效。
/// Electron 的 GetFocusedElement 常是 `Chrome_WidgetWin_1`，要再找 HasKeyboardFocus 后代。
#[cfg(windows)]
fn uia_caret_point() -> (Option<(i32, i32)>, String) {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
        IUIAutomationTextPattern2, IUIAutomationTreeWalker, TreeScope_Descendants,
        UIA_HasKeyboardFocusPropertyId, UIA_TextPattern2Id, UIA_TextPatternId,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    unsafe fn class_of(el: &IUIAutomationElement) -> String {
        el.CurrentClassName()
            .ok()
            .map(|b| b.to_string())
            .unwrap_or_default()
    }

    unsafe fn try_el(
        el: &IUIAutomationElement,
        fg_rc: Option<(i32, i32, i32, i32)>,
        wechat: bool,
        tag: &str,
    ) -> Option<((i32, i32), String)> {
        use windows::Win32::UI::Accessibility::{
            IUIAutomationTextRange, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
            TextUnit_Character,
        };
        let class_name = class_of(el);
        if class_name.contains("Chrome_RenderWidgetHostHWND")
            || class_name.contains("Chrome_WidgetWin")
        {
            return None;
        }
        let ctrl = el.CurrentBoundingRectangle().ok().map(|r| {
            (
                r.left as f64,
                r.top as f64,
                (r.right - r.left) as f64,
                (r.bottom - r.top) as f64,
            )
        });
        unsafe fn from_range(
            range: &IUIAutomationTextRange,
            class_name: &str,
            fg_rc: Option<(i32, i32, i32, i32)>,
            wechat: bool,
            ctrl: Option<(f64, f64, f64, f64)>,
        ) -> Option<((i32, i32), String)> {
            if let Some(hit) = point_from_text_range(range, class_name, fg_rc, wechat, ctrl, false) {
                return Some(hit);
            }
            let collapsed = range
                .CompareEndpoints(
                    TextPatternRangeEndpoint_Start,
                    range,
                    TextPatternRangeEndpoint_End,
                )
                .ok()
                == Some(0);
            if !collapsed {
                return None;
            }
            let cloned = range.Clone().ok()?;
            cloned.ExpandToEnclosingUnit(TextUnit_Character).ok()?;
            point_from_text_range(&cloned, class_name, fg_rc, wechat, ctrl, false)
        }
        unsafe fn last_char_caret(
            el: &IUIAutomationElement,
            class_name: &str,
            fg_rc: Option<(i32, i32, i32, i32)>,
            wechat: bool,
            ctrl: Option<(f64, f64, f64, f64)>,
        ) -> Option<((i32, i32), String)> {
            let pattern = el
                .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                .ok()?;
            let doc = pattern.DocumentRange().ok()?;
            let raw = doc.GetText(64).ok()?.to_string();
            let n = raw
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '\u{200b}' && *c != '\u{feff}')
                .count();
            if n == 0 || n > 32 {
                return None;
            }
            let end = doc.Clone().ok()?;
            end.MoveEndpointByRange(
                TextPatternRangeEndpoint_Start,
                &doc,
                TextPatternRangeEndpoint_End,
            )
            .ok()?;
            let _ = end.MoveEndpointByUnit(
                TextPatternRangeEndpoint_Start,
                TextUnit_Character,
                -1,
            );
            point_from_text_range(&end, class_name, fg_rc, wechat, ctrl, true)
        }
        let mut hit = None;
        if let Ok(pattern) =
            el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        {
            if let Ok(sel) = pattern.GetSelection() {
                if sel.Length().unwrap_or(0) > 0 {
                    if let Ok(range) = sel.GetElement(0) {
                        if let Some((pt, seen)) =
                            from_range(&range, &class_name, fg_rc, wechat, ctrl)
                        {
                            hit = Some((pt, format!("{tag}:text-sel {seen}")));
                        }
                    }
                }
            }
        }
        if hit.is_none() {
            if let Ok(p2) =
                el.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id)
            {
                let mut active = BOOL(0);
                if let Ok(range) = p2.GetCaretRange(&mut active) {
                    if let Some((pt, seen)) =
                        from_range(&range, &class_name, fg_rc, wechat, ctrl)
                    {
                        hit = Some((pt, format!("{tag}:caret-range {seen}")));
                    }
                }
            }
        }
        if let Some((pt, seen)) = hit {
            if caret_at_pad_origin(pt.0, pt.1, ctrl) {
                if let Some((ept, eseen)) =
                    last_char_caret(el, &class_name, fg_rc, wechat, ctrl)
                {
                    return Some((ept, format!("{tag}:doc-end {eseen}")));
                }
            }
            return Some((pt, seen));
        }
        None
    }

    unsafe fn find_focus(
        auto: &IUIAutomation,
        root: &IUIAutomationElement,
    ) -> Option<IUIAutomationElement> {
        let cond = auto
            .CreatePropertyCondition(UIA_HasKeyboardFocusPropertyId, &VARIANT::from(true))
            .ok()?;
        root.FindFirst(TreeScope_Descendants, &cond).ok()
    }

    unsafe fn bounds_of(el: &IUIAutomationElement) -> Option<(f64, f64, f64, f64)> {
        el.CurrentBoundingRectangle().ok().map(|r| {
            (
                r.left as f64,
                r.top as f64,
                (r.right - r.left) as f64,
                (r.bottom - r.top) as f64,
            )
        })
    }

    fn is_large_host(ctrl: Option<(f64, f64, f64, f64)>, fg_rc: Option<(i32, i32, i32, i32)>) -> bool {
        let Some((_, _, w, h)) = ctrl else {
            return false;
        };
        if h > 240.0 {
            return true;
        }
        if let Some((l, t, r, b)) = fg_rc {
            let fa = (r - l) as f64 * (b - t) as f64;
            if fa > 0.0 && w * h > fa * 0.25 {
                return true;
            }
        }
        false
    }

    unsafe fn walk_compact_caret(
        walker: &IUIAutomationTreeWalker,
        el: &IUIAutomationElement,
        fg_rc: Option<(i32, i32, i32, i32)>,
        wechat: bool,
        tag: &str,
        depth: i32,
        left: &mut i32,
        best: &mut Option<((i32, i32), String, f64)>,
    ) {
        if *left <= 0 || depth < 0 {
            return;
        }
        *left -= 1;
        let ctrl = bounds_of(el);
        let compact = ctrl
            .map(|(_, _, w, h)| h > 20.0 && h < 240.0 && w > 40.0)
            .unwrap_or(false);
        if compact {
            if let Some((pt, seen)) = try_el(el, fg_rc, wechat, tag) {
                let area = ctrl.map(|(_, _, w, h)| w * h).unwrap_or(f64::MAX);
                let better = match best {
                    None => true,
                    Some((_, _, a)) => area < *a,
                };
                if better {
                    *best = Some((pt, seen, area));
                }
            }
        }
        if let Ok(child) = walker.GetFirstChildElement(el) {
            let mut cur = Some(child);
            while let Some(node) = cur {
                walk_compact_caret(
                    walker, &node, fg_rc, wechat, tag, depth - 1, left, best,
                );
                cur = walker.GetNextSiblingElement(&node).ok();
            }
        }
    }

    unsafe {
        let co = CoInitializeEx(None, COINIT_MULTITHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(v) => v,
                Err(_) => {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    return (None, "no-uia".to_string());
                }
            };
        let wechat = is_wechat_fg();
        let fg_rc = fg_rect();
        let focused = auto.GetFocusedElement().ok();
        let hwnd_el = {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                None
            } else {
                auto.ElementFromHandle(fg).ok()
            }
        };

        let mut last_miss = "no-focused".to_string();

        if let Some(ref root) = hwnd_el {
            if let Some(child) = find_focus(&auto, root) {
                if is_bottom_composer(bounds_of(&child), fg_rc) {
                    if let Some((pt, seen)) = try_el(&child, fg_rc, wechat, "hwnd-focus") {
                        if co.is_ok() {
                            CoUninitialize();
                        }
                        return (Some(pt), seen);
                    }
                }
            }
        }

        if let Some(ref el) = focused {
            let large = is_large_host(bounds_of(el), fg_rc);
            if large {
                if let Some(child) = find_focus(&auto, el) {
                    if let Some((pt, seen)) = try_el(&child, fg_rc, wechat, "focused-desc") {
                        if co.is_ok() {
                            CoUninitialize();
                        }
                        return (Some(pt), seen);
                    }
                }
                if let Ok(walker) = auto.ControlViewWalker() {
                    let mut best = None;
                    let mut budget = 40i32;
                    walk_compact_caret(
                        &walker, el, fg_rc, wechat, "focused-walk", 5, &mut budget, &mut best,
                    );
                    if let Some((pt, seen, _)) = best {
                        if co.is_ok() {
                            CoUninitialize();
                        }
                        return (Some(pt), seen);
                    }
                }
            }
            if let Some((pt, seen)) = try_el(el, fg_rc, wechat, "focused") {
                if co.is_ok() {
                    CoUninitialize();
                }
                return (Some(pt), seen);
            }
            last_miss = format!("focused-miss cls='{}'", class_of(el));
            if let Some(child) = find_focus(&auto, el) {
                if let Some((pt, seen)) = try_el(&child, fg_rc, wechat, "focused-desc") {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    return (Some(pt), seen);
                }
                last_miss = format!("focused-desc-miss cls='{}'", class_of(&child));
            }
        }

        if let Some(ref el) = hwnd_el {
            if let Some((pt, seen)) = try_el(el, fg_rc, wechat, "hwnd") {
                if co.is_ok() {
                    CoUninitialize();
                }
                return (Some(pt), seen);
            }
            if let Some(child) = find_focus(&auto, el) {
                if let Some((pt, seen)) = try_el(&child, fg_rc, wechat, "hwnd-focus") {
                    if co.is_ok() {
                        CoUninitialize();
                    }
                    return (Some(pt), seen);
                }
                last_miss = format!("hwnd-focus-miss cls='{}'", class_of(&child));
            } else if last_miss.starts_with("no-focused") {
                last_miss = format!("hwnd-no-kb-focus cls='{}'", class_of(el));
            }
        }

        if co.is_ok() {
            CoUninitialize();
        }
        (None, last_miss)
    }
}

#[cfg(windows)]
fn gui_thread_caret() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let tid = GetWindowThreadProcessId(fg, None);
        if tid == 0 {
            return None;
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(tid, &mut info).ok()?;
        if info.hwndCaret.0.is_null() {
            return None;
        }
        if info.rcCaret.right <= info.rcCaret.left || info.rcCaret.bottom <= info.rcCaret.top {
            return None;
        }
        // 主窗客户区原点上的 1px「假 caret」不要；Chromium 的系统 caret 在窗口客户区任意处，要收。
        if info.hwndCaret == fg && info.rcCaret.left <= 2 && info.rcCaret.top <= 2 {
            return None;
        }
        let mut pt = POINT {
            x: info.rcCaret.left,
            y: info.rcCaret.bottom,
        };
        if !ClientToScreen(info.hwndCaret, &mut pt).as_bool() {
            return None;
        }
        if pt.x == 0 && pt.y == 0 {
            return None;
        }
        Some((pt.x, pt.y))
    }
}

/// Chromium 为中文 IME 调用 CreateCaret/SetCaretPos。GetCaretPos 的坐标相对 **拥有 caret 的 HWND**（hwndCaret），
/// 不能对 GetFocus() 做 ClientToScreen，否则会偏到控件/窗口左上。
#[cfg(windows)]
fn attached_caret() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCaretPos, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let fg_tid = GetWindowThreadProcessId(fg, None);
        if fg_tid == 0 {
            return None;
        }
        let my_tid = GetCurrentThreadId();
        if !AttachThreadInput(my_tid, fg_tid, true).as_bool() {
            return None;
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let _ = GetGUIThreadInfo(fg_tid, &mut info);
        let focus = GetFocus();
        let hwnd = if !info.hwndCaret.0.is_null() {
            info.hwndCaret
        } else {
            focus
        };
        let mut caret = POINT::default();
        let ok = !hwnd.0.is_null() && GetCaretPos(&mut caret).is_ok();
        let pt = if ok && (caret.x != 0 || caret.y != 0) && ClientToScreen(hwnd, &mut caret).as_bool()
        {
            Some((caret.x, caret.y))
        } else {
            None
        };
        let _ = AttachThreadInput(my_tid, fg_tid, false);
        pt
    }
}

/// 输入法候选框跟的就是这个点：ImmGetCompositionWindow / CandidateWindow。
/// 微信自绘输入、部分 Qt 不暴露 Win32 caret，但会把位置告诉 IMM。
#[cfg(windows)]
fn imm_caret_point() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::Ime::{
        ImmGetCandidateWindow, ImmGetCompositionWindow, ImmGetContext, ImmReleaseContext,
        CANDIDATEFORM, COMPOSITIONFORM, CFS_RECT,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };

    unsafe fn screen_from_client(
        hwnd: HWND,
        pt: POINT,
        rc: RECT,
        style: u32,
    ) -> Option<(i32, i32)> {
        let mut p = pt;
        if (style == CFS_RECT || (p.x == 0 && p.y == 0))
            && rc.right > rc.left
            && rc.bottom > rc.top
        {
            p.x = rc.left;
            p.y = rc.bottom;
        }
        if p.x == 0 && p.y == 0 {
            return None;
        }
        if !ClientToScreen(hwnd, &mut p).as_bool() {
            return None;
        }
        Some((p.x, p.y))
    }

    unsafe fn from_hwnd(hwnd: HWND) -> Option<(i32, i32)> {
        if hwnd.0.is_null() {
            return None;
        }
        let himc = ImmGetContext(hwnd);
        if himc.0.is_null() {
            return None;
        }
        let mut found = None;
        let mut cf = COMPOSITIONFORM::default();
        if ImmGetCompositionWindow(himc, &mut cf).as_bool() {
            found = screen_from_client(hwnd, cf.ptCurrentPos, cf.rcArea, cf.dwStyle);
        }
        if found.is_none() {
            let mut cand = CANDIDATEFORM::default();
            if ImmGetCandidateWindow(himc, 0, &mut cand).as_bool() {
                found = screen_from_client(hwnd, cand.ptCurrentPos, cand.rcArea, cand.dwStyle);
            }
        }
        let _ = ImmReleaseContext(hwnd, himc);
        found
    }

    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let tid = GetWindowThreadProcessId(fg, None);
        if tid == 0 {
            return None;
        }
        let my = GetCurrentThreadId();
        let attached = AttachThreadInput(my, tid, true).as_bool();
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let _ = GetGUIThreadInfo(tid, &mut info);
        let focus = GetFocus();
        let hwnds = [info.hwndCaret, info.hwndFocus, focus, fg];
        let mut out = None;
        for hwnd in hwnds {
            if let Some(pt) = from_hwnd(hwnd) {
                out = Some(pt);
                break;
            }
        }
        if attached {
            let _ = AttachThreadInput(my, tid, false);
        }
        out
    }
}

#[cfg(windows)]
fn place_in_work(window: &Window, logical_w: f32, logical_h: f32, x: i32, y: i32) {
    // 尺寸按锚点屏 DPI 算（对齐 WPF `GetDpiForMonitor`）：window.scale_factor 是窗口
    // 当前所在屏的，show 前窗口还在主屏，混 DPI 副屏会算错物理尺寸。
    let (work, (sx, sy)) = monitor_work_and_dpi(x, y);
    let (x, y, pw, ph) = popup_rect(x, y, logical_w, logical_h, sx, sy, work);
    window.set_size(slint::WindowSize::Physical(slint::PhysicalSize::new(
        pw as u32,
        ph as u32,
    )));
    window.set_position(slint::WindowPosition::Physical(
        slint::PhysicalPosition::new(x, y),
    ));
    // slint/winit 的 set_position 在 show / WM_DPICHANGED 期间经常被冲掉，落到 (0,0)。
    // 有 HWND 时再用 SetWindowPos 钉物理坐标（对齐 WPF ApplyPendingPositionSetWindowPos）。
    commit_hwnd_placement(window, x, y, pw, ph);
}

#[cfg(windows)]
fn commit_hwnd_placement(window: &Window, x: i32, y: i32, pw: i32, ph: i32) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    resize_hook::begin_our_pos();
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            pw.max(1),
            ph.max(1),
            SWP_NOACTIVATE,
        );
    }
    resize_hook::end_our_pos();
}

/// 预览加宽后把窗口拉回当前显示器工作区，不改锚点到光标。
#[cfg(windows)]
pub fn clamp_to_work_area(window: &Window, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            return;
        }
        let scale = window.scale_factor();
        let w = (logical_w * scale) as i32;
        let h = (logical_h * scale) as i32;
        let work = mi.rcWork;
        let mut x = rc.left;
        let mut y = rc.top;
        if x + w > work.right {
            x = work.right - w;
        }
        if y + h > work.bottom {
            y = work.bottom - h;
        }
        if x < work.left {
            x = work.left;
        }
        if y < work.top {
            y = work.top;
        }
        if x != rc.left || y != rc.top {
            window.set_position(slint::WindowPosition::Physical(
                slint::PhysicalPosition::new(x, y),
            ));
        }
    }
}

#[cfg(not(windows))]
pub fn clamp_to_work_area(_window: &Window, _logical_w: f32, _logical_h: f32) {}

/// 快速查找浮层定位：资源管理器右侧优先，放不下换左侧，再不行贴工作区右缘；
/// 垂直顶部对齐资源管理器（垂直居中偏移，对齐 WPF PositionNearExplorer）。
#[cfg(windows)]
pub fn position_near_explorer(window: &Window, frame: isize, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};

    unsafe {
        let h = HWND(frame as *mut _);
        if frame == 0 || !IsWindow(Some(h)).as_bool() {
            position_near_cursor(window, logical_w, logical_h);
            return;
        }
        let mut rc = RECT::default();
        if GetWindowRect(h, &mut rc).is_err() {
            position_near_cursor(window, logical_w, logical_h);
            return;
        }
        let monitor = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            position_near_cursor(window, logical_w, logical_h);
            return;
        }
        // 尺寸按资源管理器所在屏 DPI 算（同 place_in_work，不用窗口当前屏 scale）。
        let (mut sx, mut sy) = (window.scale_factor() as f64, window.scale_factor() as f64);
        {
            use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
            let mut dx = 96u32;
            let mut dy = 96u32;
            if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok()
                && dx != 0
                && dy != 0
            {
                sx = dx as f64 / 96.0;
                sy = dy as f64 / 96.0;
            }
        }
        let w = (logical_w as f64 * sx).round() as i32;
        let hgt = (logical_h as f64 * sy).round() as i32;
        let work = mi.rcWork;
        let mut x = if rc.right + w + 8 <= work.right {
            rc.right + 4
        } else if rc.left - w - 8 >= work.left {
            rc.left - w - 4
        } else {
            work.right - w - 16
        };
        let cy = (rc.top + rc.bottom) / 2;
        let mut y = rc.top.max(cy - hgt / 2);
        if x < work.left {
            x = work.left;
        }
        if y + hgt > work.bottom {
            y = work.bottom - hgt;
        }
        if y < work.top {
            y = work.top;
        }
        window.set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(x, y),
        ));
    }
}

#[cfg(not(windows))]
pub fn position_near_hwnd_or_cursor(
    window: &Window,
    _anchor: isize,
    logical_w: f32,
    logical_h: f32,
) {
    position_near_cursor(window, logical_w, logical_h);
}

/// 对话框/窗口居中到光标所在显示器工作区（设置窗口用）。
#[cfg(windows)]
pub fn center_on_cursor_monitor(window: &Window, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return;
        }
        let (work, (sx, sy)) = monitor_work_and_dpi(pt.x, pt.y);
        let w = (logical_w as f64 * sx).round() as i32;
        let h = (logical_h as f64 * sy).round() as i32;
        let x = work.0 + ((work.2 - work.0 - w) / 2).max(0);
        let y = work.1 + ((work.3 - work.1 - h) / 2).max(0);
        window.set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(x, y),
        ));
    }
}

#[cfg(not(windows))]
pub fn center_on_cursor_monitor(_window: &Window, _logical_w: f32, _logical_h: f32) {}

/// 对话框矩形（物理像素，左/上/右/下），窗口已销毁或取不到返回 None。
#[cfg(windows)]
pub fn dialog_rect(anchor: isize) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};

    if anchor == 0 {
        return None;
    }
    unsafe {
        let h = HWND(anchor as *mut _);
        if !IsWindow(Some(h)).as_bool() {
            return None;
        }
        let mut rc = RECT::default();
        if GetWindowRect(h, &mut rc).is_err() {
            return None;
        }
        Some((rc.left, rc.top, rc.right, rc.bottom))
    }
}

#[cfg(not(windows))]
pub fn dialog_rect(_anchor: isize) -> Option<(i32, i32, i32, i32)> {
    None
}

/// 对话框是否还活着（销毁即 false，Picker 应跟着关闭）。
#[cfg(windows)]
pub fn dialog_alive(anchor: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;
    if anchor == 0 {
        return false;
    }
    unsafe { IsWindow(Some(HWND(anchor as *mut _))).as_bool() }
}

#[cfg(not(windows))]
pub fn dialog_alive(_anchor: isize) -> bool {
    false
}

/// FileJump Picker 停靠定位（对齐 WPF `FileJumpPickerDockPlacement`）：
/// 右 → 左 → 底部居中 → 工作区夹紧（物理像素，gap=4，y 与框顶对齐）。
/// 显示器取对话框中心所在屏；anchor 无效时跟光标。
#[cfg(windows)]
pub fn position_dock_dialog(window: &Window, anchor: isize, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    if let Some((l, t, r, b)) = dialog_rect(anchor) {
        unsafe {
            let center = POINT {
                x: (l + r) / 2,
                y: (t + b) / 2,
            };
            let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
                // 尺寸按对话框所在屏 DPI 算（picker 可能还在别的屏，不用它的 scale）。
                let (mut sx, mut sy) = (
                    window.scale_factor() as f64,
                    window.scale_factor() as f64,
                );
                {
                    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
                    let mut dx = 96u32;
                    let mut dy = 96u32;
                    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok()
                        && dx != 0
                        && dy != 0
                    {
                        sx = dx as f64 / 96.0;
                        sy = dy as f64 / 96.0;
                    }
                }
                let w = (logical_w as f64 * sx).round() as i32;
                let hgt = (logical_h as f64 * sy).round() as i32;
                let work = mi.rcWork;
                let (x, y) = clipx_filejump::dock::dock_position(
                    (l, t, r, b),
                    w,
                    hgt,
                    (work.left, work.top, work.right, work.bottom),
                );
                window.set_position(slint::WindowPosition::Physical(
                    slint::PhysicalPosition::new(x, y),
                ));
                return;
            }
        }
    }
    position_near_cursor(window, logical_w, logical_h);
}

#[cfg(not(windows))]
pub fn position_dock_dialog(window: &Window, _anchor: isize, logical_w: f32, logical_h: f32) {
    position_near_cursor(window, logical_w, logical_h);
}

/// Picker 重算停靠（对话框移动/缩放跟随用，对齐 WPF `TryRealtimeDockFollow`）：
/// 用 Picker 当前物理尺寸重跑 dock 规则，位置不变则跳过；
/// 对话框已死返回 false（调用方关闭 Picker）。
/// SetWindowPos 不抢焦点不改层级（NOZORDER|NOACTIVATE|NOSIZE）。
#[cfg(windows)]
pub fn redock_picker(anchor: isize) -> bool {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    let hwnd = crate::mouse_hook::FJ_HWND.load(std::sync::atomic::Ordering::SeqCst);
    if hwnd == 0 {
        return true;
    }
    let Some((l, t, r, b)) = dialog_rect(anchor) else {
        return false;
    };
    unsafe {
        let h = HWND(hwnd as *mut _);
        let mut rc = RECT::default();
        if GetWindowRect(h, &mut rc).is_err() {
            return true;
        }
        let pw = rc.right - rc.left;
        let ph = rc.bottom - rc.top;
        if pw <= 0 || ph <= 0 {
            return true;
        }
        let center = POINT {
            x: (l + r) / 2,
            y: (t + b) / 2,
        };
        let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFO).as_bool() {
            return true;
        }
        let work = mi.rcWork;
        let (x, y) = clipx_filejump::dock::dock_position(
            (l, t, r, b),
            pw,
            ph,
            (work.left, work.top, work.right, work.bottom),
        );
        if x == rc.left && y == rc.top {
            return true;
        }
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        true
    }
}

#[cfg(not(windows))]
pub fn redock_picker(_anchor: isize) -> bool {
    true
}

/// Picker 手动拖拽：逻辑位移（逻辑像素，内部乘 scale）。
#[cfg(windows)]
pub fn drag_picker_by(window: &Window, dx_logical: f32, dy_logical: f32) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
    };

    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return;
        }
        let scale = window.scale_factor();
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rc.left + (dx_logical * scale) as i32,
            rc.top + (dy_logical * scale) as i32,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

#[cfg(not(windows))]
pub fn drag_picker_by(_window: &Window, _dx: f32, _dy: f32) {}

#[cfg(windows)]
pub fn store_hwnd(window: &Window) {
    if let Some(h) = hwnd_isize(window) {
        crate::mouse_hook::POPUP_HWND.store(h, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(windows)]
pub fn foreground_hwnd() -> isize {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow().0 as isize }
}

#[cfg(windows)]
pub fn foreground_is_file_dialog() -> bool {
    let h = foreground_hwnd();
    h != 0
        && clipx_filejump::dialog::win::classify_hwnd(h)
            .map(|k| k != clipx_filejump::DialogKind::NotDialog)
            .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn foreground_is_file_dialog() -> bool {
    false
}

/// 快速查找浮层定位（非 Windows 无资源管理器停靠语义，no-op）。
#[cfg(not(windows))]
pub fn position_near_explorer(_window: &Window, _frame: isize, _logical_w: f32, _logical_h: f32) {}

/// 粘贴前把前台抢回呼出时的目标窗（对齐 WPF SetForegroundWindowAggressive）。
#[cfg(windows)]
pub fn restore_foreground(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SetForegroundWindow,
    };

    if hwnd == 0 {
        return;
    }
    let target = HWND(hwnd as *mut _);
    unsafe {
        if !IsWindow(Some(target)).as_bool() {
            return;
        }
        let fg = GetForegroundWindow();
        if fg == target {
            return;
        }
        let cur = GetCurrentThreadId();
        let mut fg_pid = 0u32;
        let fg_tid = if fg.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, Some(&mut fg_pid))
        };
        let mut target_pid = 0u32;
        let target_tid = GetWindowThreadProcessId(target, Some(&mut target_pid));
        let attach_fg = fg_tid != 0 && fg_tid != cur;
        let attach_target = target_tid != 0 && target_tid != cur && target_tid != fg_tid;
        if attach_fg {
            let _ = AttachThreadInput(cur, fg_tid, true);
        }
        if attach_target {
            let _ = AttachThreadInput(cur, target_tid, true);
        }
        let _ = SetForegroundWindow(target);
        if attach_target {
            let _ = AttachThreadInput(cur, target_tid, false);
        }
        if attach_fg {
            let _ = AttachThreadInput(cur, fg_tid, false);
        }
    }
}

/// 普通窗口（设置窗口）show 后确保到最前：winit 的 show 不绕过前台锁，
/// 呼出时前台是别的进程，窗口会落在底部或被遮挡。
///
/// 两个坑：一是新建窗口后 winit 仍会补一次 SetWindowPos（首帧/DPI 落定），
/// 过早置顶会被它覆盖回底部，故延迟到窗口稳定后再动手；二是本进程若从未当过
/// 前台进程（首次打开），SetForegroundWindow 会被驳回，需 Alt 键 hack 造一次
/// 用户输入。拿到前台后降回普通 z 序；始终拿不到就保持 TOPMOST 兜底
///（窗口生命周期短，关闭即销毁，不会长期挡住别的窗口）。
#[cfg(windows)]
pub fn activate_window(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    std::thread::Builder::new()
        .name("clipx-activate".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            try_activate(hwnd);
            if foreground_is(hwnd) {
                release_topmost(hwnd);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
            try_activate(hwnd);
            if foreground_is(hwnd) {
                release_topmost(hwnd);
            }
        })
        .ok();
}

#[cfg(windows)]
fn foreground_is(hwnd: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow().0 as isize == hwnd }
}

/// 枚举本进程可见顶层窗口，按标题子串找 HWND（FindWindowW 在本工程环境
/// 实测报 error 0x800700CB，不可用；Slint 的 raw_window_handle 也拿不到）。
#[cfg(windows)]
pub fn find_window_by_title(needle: &str) -> Option<isize> {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible,
    };

    static RESULT: AtomicIsize = AtomicIsize::new(0);
    static NEEDLE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    if let Ok(mut n) = NEEDLE.lock() {
        *n = needle.to_lowercase();
    }
    RESULT.store(0, Ordering::SeqCst);

    unsafe extern "system" fn on_window(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        if RESULT.load(Ordering::SeqCst) != 0 {
            return TRUE;
        }
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != std::process::id() {
            return TRUE;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return TRUE;
        }
        let mut buf = [0u16; 128];
        let n = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        if let Ok(needle) = NEEDLE.lock() {
            if title.to_lowercase().contains(needle.as_str()) {
                RESULT.store(hwnd.0 as isize, Ordering::SeqCst);
            }
        }
        TRUE
    }

    unsafe {
        let _ = EnumWindows(Some(on_window), LPARAM(0));
    }
    match RESULT.load(Ordering::SeqCst) {
        0 => None,
        v => Some(v),
    }
}

#[cfg(windows)]
fn release_topmost(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, SWP_NOMOVE, SWP_NOSIZE, SWP_NOACTIVATE,
    };
    unsafe {
        let _ = SetWindowPos(
            HWND(hwnd as *mut _),
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

#[cfg(windows)]
fn try_activate(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP, VK_MENU};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, SetForegroundWindow, SetWindowPos, HWND_TOPMOST,
        SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };

    let h = HWND(hwnd as *mut _);
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = BringWindowToTop(h);
        let _ = SetForegroundWindow(h);
        restore_foreground(hwnd);
        if GetForegroundWindow() == h {
            return;
        }
        // 造一次用户输入，让系统放行前台切换。
        let _ = keybd_event(VK_MENU.0 as u8, 0, Default::default(), 0);
        let _ = keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        restore_foreground(hwnd);
        let _ = SetForegroundWindow(h);
    }
}

#[cfg(not(windows))]
pub fn activate_window(_hwnd: isize) {}

/// 空闲时归还工作集给 OS（经典 SetProcessWorkingSetSize(-1,-1) 惯用法）。
/// 峰值操作（4K 预览解码等）后防止 WS 虚高；下次访问 soft fault 廉价拉回。
#[cfg(windows)]
pub fn trim_working_set() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    unsafe {
        let h = GetCurrentProcess();
        let _ = SetProcessWorkingSetSize(h, usize::MAX, usize::MAX);
    }
}

#[cfg(not(windows))]
pub fn trim_working_set() {}

#[cfg(windows)]
fn hwnd_isize(window: &Window) -> Option<isize> {
    hwnd_of(window).map(|h| h.0 as isize)
}

#[cfg(windows)]
fn hwnd_of(window: &Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().window_handle() {
        Ok(handle) => match handle.as_raw() {
            RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut _)),
            _ => None,
        },
        Err(_) => None,
    }
}

#[cfg(not(windows))]
pub fn apply_style(_window: &Window) {}

#[cfg(not(windows))]
pub fn position_near_cursor(_window: &Window, _logical_w: f32, _logical_h: f32) {}

#[cfg(not(windows))]
pub fn store_hwnd(_window: &Window) {}

#[cfg(not(windows))]
pub fn foreground_hwnd() -> isize {
    0
}

/// 边缘拖拽改尺寸（对齐 WPF `WindowResizeHelper`）。
#[cfg(windows)]
mod resize_hook {
    use super::hwnd_of;
    use slint::Window;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::Mutex;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, GWLP_WNDPROC,
        HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT,
        MA_ACTIVATE, MA_NOACTIVATE, SWP_NOMOVE, WINDOWPOS, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE,
        WM_MOUSEACTIVATE, WM_NCHITTEST, WM_SIZING, WM_WINDOWPOSCHANGING,
    };

    const BORDER: f32 = 8.0;
    const MARGIN: f32 = 16.0;
    static ORIG: AtomicIsize = AtomicIsize::new(0);
    static HOOKED: AtomicIsize = AtomicIsize::new(0);
    static RESIZING: AtomicBool = AtomicBool::new(false);
    static EDIT_ACTIVE: AtomicBool = AtomicBool::new(false);
    static POS_LOCK: AtomicBool = AtomicBool::new(false);
    static OUR_POS: AtomicBool = AtomicBool::new(false);
    static HANDLER: Mutex<Option<Box<dyn Fn(f32, f32) + Send>>> = Mutex::new(None);

    pub fn set_handler(f: impl Fn(f32, f32) + Send + 'static) {
        *HANDLER.lock().unwrap() = Some(Box::new(f));
    }

    pub fn is_resizing() -> bool {
        RESIZING.load(Ordering::SeqCst)
    }

    pub fn set_edit_active(v: bool) {
        EDIT_ACTIVE.store(v, Ordering::SeqCst);
    }

    pub fn set_placement_lock(v: bool) {
        POS_LOCK.store(v, Ordering::SeqCst);
    }

    pub fn begin_our_pos() {
        OUR_POS.store(true, Ordering::SeqCst);
    }

    pub fn end_our_pos() {
        OUR_POS.store(false, Ordering::SeqCst);
    }

    pub fn install(window: &Window) {
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        let h = hwnd.0 as isize;
        unsafe {
            let orig = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
            if orig == wndproc as *const () as usize as isize {
                HOOKED.store(h, Ordering::SeqCst);
                return;
            }
            ORIG.store(orig, Ordering::SeqCst);
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as usize as isize);
            HOOKED.store(h, Ordering::SeqCst);
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCHITTEST => {
                if let Some(ht) = hit_test(hwnd, lparam) {
                    return LRESULT(ht);
                }
            }
            WM_SIZING => {
                clamp_sizing(hwnd, wparam, lparam);
                RESIZING.store(true, Ordering::SeqCst);
            }
            WM_ENTERSIZEMOVE => {
                RESIZING.store(true, Ordering::SeqCst);
            }
            WM_EXITSIZEMOVE => {
                RESIZING.store(false, Ordering::SeqCst);
                emit_size(hwnd);
            }
            WM_MOUSEACTIVATE => {
                let code = if EDIT_ACTIVE.load(Ordering::SeqCst) {
                    MA_ACTIVATE
                } else {
                    MA_NOACTIVATE
                };
                return LRESULT(code as isize);
            }
            WM_WINDOWPOSCHANGING => {
                // 对齐 WPF `_lockPopupWindowNomove`：show / DPI 切换期间禁止壳把窗口拽回 (0,0)。
                // 我们自己的 SetWindowPos（OUR_POS）以及拖尺寸必须放行。
                if POS_LOCK.load(Ordering::SeqCst)
                    && !OUR_POS.load(Ordering::SeqCst)
                    && !RESIZING.load(Ordering::SeqCst)
                    && lparam.0 != 0
                {
                    let pos = lparam.0 as *mut WINDOWPOS;
                    (*pos).flags |= SWP_NOMOVE;
                }
            }
            _ => {}
        }
        let orig = ORIG.load(Ordering::SeqCst);
        CallWindowProcW(Some(std::mem::transmute(orig)), hwnd, msg, wparam, lparam)
    }

    unsafe fn hit_test(hwnd: HWND, lparam: LPARAM) -> Option<isize> {
        let packed = lparam.0 as u32;
        let sx = (packed & 0xFFFF) as i16 as i32;
        let sy = ((packed >> 16) & 0xFFFF) as i16 as i32;
        let mut rc = RECT::default();
        GetWindowRect(hwnd, &mut rc).ok()?;
        let dpi = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let rel_x = (sx - rc.left) as f32 / dpi;
        let rel_y = (sy - rc.top) as f32 / dpi;
        let win_w = (rc.right - rc.left) as f32 / dpi;
        let win_h = (rc.bottom - rc.top) as f32 / dpi;
        if rel_x >= MARGIN && rel_x < win_w - MARGIN && rel_y >= MARGIN && rel_y < win_h - MARGIN {
            return None;
        }
        let left = rel_x < BORDER;
        let right = rel_x >= win_w - BORDER;
        let top = rel_y < BORDER;
        let bottom = rel_y >= win_h - BORDER;
        let ht = match (left, right, top, bottom) {
            (true, _, true, _) => HTTOPLEFT,
            (_, true, true, _) => HTTOPRIGHT,
            (true, _, _, true) => HTBOTTOMLEFT,
            (_, true, _, true) => HTBOTTOMRIGHT,
            (true, _, _, _) => HTLEFT,
            (_, true, _, _) => HTRIGHT,
            (_, _, true, _) => HTTOP,
            (_, _, _, true) => HTBOTTOM,
            _ => return None,
        };
        Some(ht as isize)
    }

    unsafe fn clamp_sizing(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
        if lparam.0 == 0 {
            return;
        }
        let rc = lparam.0 as *mut RECT;
        let dpi = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let mut r = *rc;
        let w = ((r.right - r.left) as f32 / dpi).clamp(312.0, 1232.0);
        let h = ((r.bottom - r.top) as f32 / dpi).clamp(232.0, 932.0);
        let pw = (w * dpi).round() as i32;
        let ph = (h * dpi).round() as i32;
        let edge = wparam.0 as i32;
        const WMSZ_LEFT: i32 = 1;
        const WMSZ_RIGHT: i32 = 2;
        const WMSZ_TOP: i32 = 3;
        const WMSZ_TOPLEFT: i32 = 4;
        const WMSZ_TOPRIGHT: i32 = 5;
        const WMSZ_BOTTOM: i32 = 6;
        const WMSZ_BOTTOMLEFT: i32 = 7;
        const WMSZ_BOTTOMRIGHT: i32 = 8;
        match edge {
            WMSZ_RIGHT | WMSZ_TOPRIGHT | WMSZ_BOTTOMRIGHT => r.right = r.left + pw,
            WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT => r.left = r.right - pw,
            _ => {}
        }
        match edge {
            WMSZ_BOTTOM | WMSZ_BOTTOMLEFT | WMSZ_BOTTOMRIGHT => r.bottom = r.top + ph,
            WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT => r.top = r.bottom - ph,
            _ => {}
        }
        *rc = r;
    }

    unsafe fn emit_size(hwnd: HWND) {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return;
        }
        let dpi = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let w = (rc.right - rc.left) as f32 / dpi;
        let h = (rc.bottom - rc.top) as f32 / dpi;
        if let Some(cb) = HANDLER.lock().unwrap().as_ref() {
            cb(w, h);
        }
    }
}

#[cfg(windows)]
pub fn set_resize_handler(on_size: impl Fn(f32, f32) + Send + 'static) {
    resize_hook::set_handler(on_size);
}

#[cfg(not(windows))]
pub fn set_resize_handler(_on_size: impl Fn(f32, f32) + Send + 'static) {}

#[cfg(windows)]
#[allow(dead_code)]
pub fn install_resize_hook(window: &Window, on_size: impl Fn(f32, f32) + Send + 'static) {
    resize_hook::set_handler(on_size);
    resize_hook::install(window);
}

#[cfg(not(windows))]
pub fn install_resize_hook(_window: &Window, _on_size: impl Fn(f32, f32) + Send + 'static) {}

#[cfg(windows)]
pub fn ensure_resize_hook(window: &Window) {
    resize_hook::install(window);
}

#[cfg(not(windows))]
pub fn ensure_resize_hook(_window: &Window) {}

/// show 前后锁位置：拦截壳/winit 在 WM_WINDOWPOSCHANGING 里把窗口拽回主屏左上。
#[cfg(windows)]
pub fn lock_placement(on: bool) {
    resize_hook::set_placement_lock(on);
}

#[cfg(not(windows))]
pub fn lock_placement(_on: bool) {}

#[cfg(windows)]
pub fn is_resizing() -> bool {
    resize_hook::is_resizing()
}

#[cfg(not(windows))]
pub fn is_resizing() -> bool {
    false
}

/// 编辑文本时去掉 WS_EX_NOACTIVATE 并激活窗口，便于 IME / TextInput。
#[cfg(windows)]
pub fn set_edit_activate(window: &Window, on: bool) {
    resize_hook::set_edit_active(on);
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_ex = if on {
            current & !(WS_EX_NOACTIVATE.0 as isize)
        } else {
            current | (WS_EX_NOACTIVATE.0 as isize)
        };
        if new_ex != current {
            let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
            let mut flags = SWP_NOMOVE | SWP_NOSIZE | SWP_FRAMECHANGED | SWP_SHOWWINDOW;
            if !on {
                flags |= SWP_NOACTIVATE;
            }
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        }
        if on {
            use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
pub fn set_edit_activate(_window: &Window, _on: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: (i32, i32, i32, i32) = (0, 0, 1920, 1040);

    #[test]
    fn anchor_kept_when_fits() {
        // 锚点 (500,500)，400x300 @100%：原样落。
        assert_eq!(popup_rect(500, 500, 400.0, 300.0, 1.0, 1.0, WORK), (500, 500, 400, 300));
    }

    #[test]
    fn right_overflow_clings_to_work_right() {
        // x+400 > 1920 → 右贴边。
        assert_eq!(
            popup_rect(1700, 100, 400.0, 300.0, 1.0, 1.0, WORK),
            (1520, 100, 400, 300)
        );
    }

    #[test]
    fn bottom_overflow_prefers_above() {
        // 下方放不下且上方能放下：翻到 caret 上方，露出底栏输入。
        assert_eq!(
            popup_rect(100, 900, 400.0, 300.0, 1.0, 1.0, WORK),
            (100, 588, 400, 300)
        );
    }

    #[test]
    fn bottom_overflow_above_clamps_right() {
        assert_eq!(
            popup_rect(1700, 900, 400.0, 300.0, 1.0, 1.0, WORK),
            (1520, 588, 400, 300)
        );
    }

    #[test]
    fn above_does_not_escape_work_top() {
        // 锚点贴底且弹窗高：上方也放不下 → 夹到工作区顶部。
        assert_eq!(popup_rect(100, 1030, 400.0, 1200.0, 1.0, 1.0, WORK).1, 0);
    }

    #[test]
    fn hidpi_scales_physical_size() {
        // 150% 屏：400x300 逻辑 → 600x450 物理。
        assert_eq!(
            popup_rect(100, 100, 400.0, 300.0, 1.5, 1.5, WORK),
            (100, 100, 600, 450)
        );
    }

    #[test]
    fn negative_origin_monitor() {
        // 副屏在主屏左侧（工作区原点为负）：锚点 (-1500,400) 正常落，不被夹回主屏。
        let work = (-1920, 0, 0, 1040);
        assert_eq!(
            popup_rect(-1500, 400, 400.0, 300.0, 1.0, 1.0, work),
            (-1500, 400, 400, 300)
        );
    }

    #[test]
    fn origin_noise_text_sel_rejected() {
        assert!(!uia_text_sel_ok(0.0, 0.0, 0.0, 16.0, None, false));
        assert!(!uia_text_sel_ok(0.2, 0.4, 0.0, 18.0, None, false));
        assert!(!uia_text_sel_ok(0.0, 0.0, 1.0, 0.0, None, false));
    }

    #[test]
    fn negative_monitor_caret_accepted() {
        let fg = (-1920, 0, 0, 1080);
        assert!(uia_text_sel_ok(-1500.0, 400.0, 1.0, 18.0, Some(fg), false));
    }

    #[test]
    fn origin_caret_not_on_left_secondary() {
        // 左副屏右缘是 x=0：主屏原点不算落在副屏前台窗内。
        let fg = (-1920, 0, 0, 1080);
        assert!(!uia_text_sel_ok(0.0, 24.0, 1.0, 18.0, Some(fg), false));
        assert!(!uia_text_sel_ok(0.5, 0.5, 1.0, 18.0, Some(fg), false));
    }

    #[test]
    fn real_caret_on_primary_accepted() {
        let fg = (0, 0, 1920, 1080);
        assert!(uia_text_sel_ok(120.0, 200.0, 1.0, 18.0, Some(fg), false));
    }

    #[test]
    fn electron_empty_cls_caret_sized() {
        assert!(uia_rect_is_caret_sized(1.0, 18.0));
        assert!(uia_rect_is_caret_sized(2.0, 22.0));
        assert!(!uia_rect_is_caret_sized(1920.0, 1032.0));
        // 整块输入框不是 caret，不能拿来当锚点（否则弹窗盖住真正的插入点）。
        assert!(!uia_rect_is_caret_sized(400.0, 80.0));
        assert!(!uia_rect_is_caret_sized(800.0, 48.0));
        assert!(!uia_rect_is_caret_sized(1.0, 56.0));
        assert!(!uia_rect_is_caret_sized(0.0, 50.0));
        assert!(uia_rect_is_caret_sized(1.0, 22.0));
    }

    #[test]
    fn control_box_range_rejected() {
        let input = Some((100.0, 800.0, 720.0, 56.0));
        assert!(uia_range_is_control_box(100.0, 800.0, 720.0, 56.0, input));
        assert!(uia_range_is_control_box(100.0, 800.0, 1.0, 56.0, input));
        assert!(uia_range_is_control_box(100.0, 836.0, 2.0, 20.0, input));
        assert!(!uia_range_is_control_box(320.0, 818.0, 2.0, 18.0, input));
    }

    #[test]
    fn pad_origin_of_wide_input() {
        let input = Some((3500.0, 460.0, 800.0, 180.0));
        assert!(caret_at_pad_origin(3506, 497, input));
        assert!(!caret_at_pad_origin(3720, 497, input));
    }

    #[test]
    fn bottom_composer_in_lower_window() {
        let fg = Some((2867, -14, 4813, 1044));
        let mid = Some((3400.0, 450.0, 800.0, 80.0));
        let bottom = Some((3200.0, 860.0, 900.0, 120.0));
        assert!(!is_bottom_composer(mid, fg));
        assert!(is_bottom_composer(bottom, fg));
    }

    #[test]
    fn workbuddy_mid_caret_snaps_to_window_bottom() {
        let fg = (2867, -14, 4813, 1044);
        assert_eq!(
            snap_mid_window_caret_to_bottom((3506, 505), fg, true),
            Some((3506, 924))
        );
        assert_eq!(
            snap_mid_window_caret_to_bottom((3506, 920), fg, true),
            None
        );
        assert_eq!(
            snap_mid_window_caret_to_bottom((3506, 505), fg, false),
            None
        );
    }

    #[test]
    fn mid_screen_stays_above_caret_x() {
        let work = (2880, -1, 4800, 1031);
        assert_eq!(
            popup_rect(3506, 505, 452.0, 592.0, 1.0, 1.0, work),
            (3506, -1, 452, 592)
        );
    }

    #[test]
    fn workbuddy_popup_sticks_to_input_box() {
        let fg = (2867, -14, 4813, 1044);
        let work = (2880, -1, 4800, 1031);
        let box_rc = estimate_large_input_box(fg);
        assert!(box_rc.1 > 800, "input is at window bottom, top={}", box_rc.1);
        let (x, y, pw, ph) = popup_rect_on_box(box_rc, 452.0, 592.0, 1.0, 1.0, work, Some(fg));
        let cx = box_rc.0 + (box_rc.2 - box_rc.0 - pw) / 2;
        assert_eq!(x, cx, "must center on the input");
        assert!(y + ph <= box_rc.1, "must sit above the bottom input");
        assert_eq!(composer_box_anchor((3200.0, 860.0, 900.0, 120.0)), (3216, 860));
    }

    #[test]
    fn workbuddy_chat_window_popup_goes_above() {
        // 现场：800x600 对话窗，输入在底部，UIA 曾判 no-composer 后贴地。
        let fg = (3397, 261, 4197, 861);
        let work = (2880, -1, 4800, 1031);
        let box_rc = estimate_large_input_box(fg);
        assert!(box_rc.1 > 700, "bottom composer, top={}", box_rc.1);
        let (_, y, _, ph) = popup_rect_on_box(box_rc, 452.0, 592.0, 1.0, 1.0, work, Some(fg));
        assert!(y + ph <= box_rc.1, "above the input, y={y} bottom={}", y + ph);
        assert!(y < 300, "not glued to work bottom, y={y}");
    }

    #[test]
    fn workbuddy_uia_caret_becomes_input_box() {
        let fg = (2867, -14, 4813, 1044);
        let work = (2880, -1, 4800, 1031);
        let box_rc = box_from_insert_pt((3506, 476), fg);
        assert_eq!(box_rc.0, 3506);
        let (x, y, pw, _) = popup_rect_on_box(box_rc, 452.0, 592.0, 1.0, 1.0, work, Some(fg));
        let cx = box_rc.0 + (box_rc.2 - box_rc.0 - pw) / 2;
        assert_eq!(x, cx);
        assert_eq!(y, work.3 - 592);
        assert!(is_usable_composer(Some((3400.0, 380.0, 900.0, 140.0)), fg));
        assert!(!is_usable_composer(Some((2867.0, -14.0, 1946.0, 1058.0)), fg));
        assert!(is_usable_composer(Some((3413.0, 780.0, 720.0, 64.0)), (3397, 261, 4197, 861)));
    }

    #[test]
    fn workbuddy_small_window_popup_centered() {
        let fg = (3704, 117, 4504, 717);
        let work = (2880, -1, 4800, 1031);
        let box_rc = estimate_large_input_box(fg);
        let (x, _, pw, _) = popup_rect_on_box(box_rc, 452.0, 592.0, 1.0, 1.0, work, Some(fg));
        let mid = (box_rc.0 + box_rc.2) / 2;
        assert!((x + pw / 2 - mid).abs() < 8, "x={x} mid={mid}");
    }

    #[test]
    fn workbuddy_small_window_popup_above_bottom_input() {
        let fg = (3183, 2, 3983, 602);
        let work = (2880, -1, 4800, 1031);
        let box_rc = estimate_large_input_box(fg);
        let (_, y, _, ph) = popup_rect_on_box(box_rc, 452.0, 592.0, 1.0, 1.0, work, Some(fg));
        assert!(y < box_rc.1, "must sit above the input, y={y} box={}", box_rc.1);
        assert_ne!(y, work.3 - ph, "must not glue to work bottom, y={y}");
    }

    #[test]
    fn chromium_token_matches_electron_host() {
        assert!(is_chromium_token("Chrome_WidgetWin_1"));
        assert!(is_chromium_token("Chrome_RenderWidgetHostHWND"));
        assert!(!is_chromium_token("Weixin.exe"));
        assert!(!is_chromium_token("WorkBuddy.exe"));
    }

    #[test]
    fn wechat_token_and_real_input_point() {
        assert!(is_wechat_token("Weixin.exe"));
        assert!(is_wechat_token("mmui::ChatInputField"));
        let fg = (0, 0, 1920, 1080);
        assert!(caret_point_usable(480, 920, Some(fg), true));
    }

    #[test]
    fn caret_in_fg_anywhere_ok() {
        let fg = (-1920, 0, 0, 1080);
        assert!(caret_point_usable(-1600, 80, Some(fg), true));
        assert!(caret_point_usable(-800, 900, Some(fg), true));
    }
}
