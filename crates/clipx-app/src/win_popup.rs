use slint::Window;
use std::sync::atomic::{AtomicBool, Ordering};

/// WorkBuddy 等追不到真 caret 时：锚点是输入大框左上，禁止再翻到屏幕顶。
static PLACE_ON_BOX: AtomicBool = AtomicBool::new(false);
static INPUT_BOX: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);
static HOST_FG: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);

/// 自检注入：强制 `is_shell_foreground()` 返回 true（`--shell-demo`）。
/// 开始菜单前台这条件外部往返才能出现，不注入则本机以外无法稳定复现 Shell 定位分支。
static SHELL_DEMO_FORCE: AtomicBool = AtomicBool::new(false);

/// 自检用：见 `SHELL_DEMO_FORCE`。
pub fn set_shell_demo(on: bool) {
    SHELL_DEMO_FORCE.store(on, Ordering::SeqCst);
}

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

/// 右下角提示条（toast）显示：定位到光标所在显示器工作区右下角（边距 12px，
/// 天然避开任务栏），再以「不抢焦点」显示。
///
/// 窗口扩展样式已由 `apply_style` 打好（NOACTIVATE | TOOLWINDOW | TOPMOST | LAYERED），
/// 这里只管几何与显隐；DPI 按锚点屏取（`monitor_work_and_dpi`），混 DPI 副屏也算得准。
#[cfg(windows)]
pub fn show_toast(window: &Window, logical_w: f32, logical_h: f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return;
    }
    let (work, (sx, sy)) = monitor_work_and_dpi(pt.x, pt.y);
    let (l, t, r, b) = work;
    const MARGIN: i32 = 12;
    // 只算位置：尺寸必须留给 Slint/winit（它把 `width: 400px` 这类**逻辑**尺寸
    // 按 scale 换算成物理）。这里若也用 SetWindowPos 钉物理尺寸，在 200% 缩放下
    // 会把 400x108 逻辑的窗口压成 400x108 物理 = 200x54 逻辑，内容只剩左上四分之一。
    let pw = (logical_w as f64 * sx).round() as i32;
    let ph = (logical_h as f64 * sy).round() as i32;
    let x = (r - pw - MARGIN).max(l);
    let y = (b - ph - MARGIN).max(t);
    window.show().ok();
    window.set_position(slint::WindowPosition::Physical(slint::PhysicalPosition::new(
        x, y,
    )));
    // show 期间 winit 常把位置重置到 (0,0)，有 HWND 时再钉一次（只动位置）。
    commit_hwnd_pos_only(window, x, y);
}

/// 只钉位置、不动尺寸（`SWP_NOSIZE`），用于尺寸由 Slint 逻辑值决定的窗口。
#[cfg(windows)]
fn commit_hwnd_pos_only(window: &Window, x: i32, y: i32) {
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
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    resize_hook::end_our_pos();
}

#[cfg(not(windows))]
pub fn show_toast(window: &Window, logical_w: f32, logical_h: f32) {
    let _ = (logical_w, logical_h);
    window.show().ok();
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

/// 常驻浮窗 hide→show 后「首帧残缺」的修复：强制下一次渲染整窗重绘。
///
/// ## 根因（Slint 1.17 软件渲染器 + softbuffer Win32 后端）
///
/// `i-slint-backend-winit/renderer/sw.rs:111` 按 softbuffer 的 `buffer.age()` 选重绘策略：
/// 窗口重新显示时 surface 被复用 → `age()==1` → `RepaintBufferType::ReusedBuffer`，
/// 此时 Slint **只重绘脏区**（`i-slint-core/partial_renderer.rs::apply_dirty_region`）。
/// 而常驻浮窗是 `hide()` 后再 `show()`，新映射的 surface 内容未定义，
/// 卡片背景 / 表头 / 分隔线 / 底栏这些「本次属性没变化」的元素不在脏区里，
/// 于是整块保持空白 —— 现象正是「表头和底栏凭空消失，只剩输入框」，
/// 等结果行到达把脏区撑大后才「自己好起来」。
///
/// 为什么 `hide()` 之后 `age()` 仍是 1：softbuffer 的 `Win32Impl` 只在 `resize()`
/// 真正换尺寸时重建缓冲（`softbuffer-0.4.8/src/backends/win32.rs:253` 同尺寸直接
/// `return Ok(())`），且 `age()`（同文件 :308）只判断 `buffer.presented` ——
/// 它根本不知道窗口被隐藏过。
///
/// ## 采用的方案：先按「高度 -1」显示一帧，再恢复目标尺寸
///
/// 关键约束：`sw.rs::render()` 每帧都用**当前窗口尺寸**调 `surface.resize()`，
/// 只有与上次 present 的尺寸不同时才会重建缓冲并让 `age()` 归 0。
/// 因此「抖动」必须**真的被某一帧看到**，不能在同一次事件循环里抖动后立刻改回
/// （那样首帧看到的仍是原尺寸，缓冲不重建，等于没做）。
///
/// 本函数做前半段：显示前把高度写小 1px。调用方随后 `show()`，
/// 首帧即以小 1px 的尺寸渲染 → 缓冲重建 → `age()==0` → `NewBuffer` 全量重绘。
/// 恢复目标尺寸由 [`restore_size_after_show`] 在下一帧完成（同样是重建缓冲 + 全量重绘，
/// 所以不会有半帧撕裂，1px 差异肉眼不可见）。
///
/// 必须在 `show()` **之前**调用。
pub fn force_full_repaint_before_show(window: &Window, logical_w: f32, logical_h: f32) {
    let scale = window.scale_factor().max(1.0);
    let pw = (logical_w * scale).round().max(1.0) as u32;
    let ph = (logical_h * scale).round().max(1.0) as u32;
    let jiggle = if ph > 1 { ph - 1 } else { ph + 1 };
    window.set_size(slint::WindowSize::Physical(slint::PhysicalSize::new(
        pw, jiggle,
    )));
}

/// [`force_full_repaint_before_show`] 的后半段：`show()` 之后延迟到下一帧再恢复目标尺寸。
///
/// 用 `Timer::single_shot(0)` 而非直接调用：必须让「小 1px 的那一帧」先真正渲染出来
/// （缓冲已按小尺寸重建、`age()` 归 0），此时再把尺寸改回目标值才会**再次**重建缓冲，
/// 从而第二次全量重绘 —— 两次都是整窗绘制，所以画面不会出现中间态。
/// 直接在 `show()` 后同步改回则两次 `set_size` 落在同一帧，等于没抖动（见前者注释）。
///
/// 传组件 weak 而非 `&Window`：`slint::Window` 不是 `Clone`，持不到定时器闭包里。
pub fn restore_size_after_show<C: slint::ComponentHandle + 'static>(
    ui: &slint::Weak<C>,
    logical_w: f32,
    logical_h: f32,
) {
    let weak = ui.clone();
    slint::Timer::single_shot(std::time::Duration::from_millis(0), move || {
        let Some(ui) = weak.upgrade() else { return };
        let win = ui.window();
        let scale = win.scale_factor().max(1.0);
        win.set_size(slint::WindowSize::Physical(slint::PhysicalSize::new(
            (logical_w * scale).round().max(1.0) as u32,
            (logical_h * scale).round().max(1.0) as u32,
        )));
    });
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
                // 关键：work/dpi 必须按 **输入框中心** 所在屏重取，不能用锚点屏。
                // 跨屏时（WorkBuddy 主窗横跨两块屏）锚点判屏与输入框实际所在屏不一致，
                // 会把弹窗 clamp 到错误屏（pos_debug 实锤 rect 甩到主屏右端）。
                #[cfg(windows)]
                let (work, (sx, sy)) = {
                    let cx = box_rc.0 + (box_rc.2 - box_rc.0) / 2;
                    let cy = box_rc.1 + (box_rc.3 - box_rc.1) / 2;
                    monitor_work_and_dpi(cx, cy)
                };
                #[cfg(not(windows))]
                let (work, (sx, sy)) = (work, (scale_x, scale_y));
                let _ = (scale_x, scale_y);
                return popup_rect_on_box(box_rc, logical_w, logical_h, sx, sy, work, host);
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
/// 2026-09-22 晚：`work` 由调用方（`popup_rect` 的 PLACE_ON_BOX 分支）**按输入框中心**
/// 重取，避免跨屏错屏；未特判应用也一并受益。
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

/// IUIAutomationTextRange::GetBoundingRectangles 的首矩形提取 (x, y, w, h)。
/// 空 SafeArray（collapsed range 未扩展时的常态）返回 None。
#[cfg(windows)]
fn text_range_first_rect(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetUBound, SafeArrayUnaccessData,
    };
    unsafe {
        let psa = range.GetBoundingRectangles().ok()?;
        if psa.is_null() {
            return None;
        }
        let ub = SafeArrayGetUBound(psa, 1).ok()?;
        if ub < 3 {
            let _ = SafeArrayDestroy(psa);
            return None;
        }
        let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
        if SafeArrayAccessData(psa, &mut data).is_err() || data.is_null() {
            let _ = SafeArrayDestroy(psa);
            return None;
        }
        let nums = data as *const f64;
        let out = (
            (*nums.add(0)).round() as i32,
            (*nums.add(1)).round() as i32,
            (*nums.add(2)).round() as i32,
            (*nums.add(3)).round() as i32,
        );
        let _ = SafeArrayUnaccessData(psa);
        let _ = SafeArrayDestroy(psa);
        Some(out)
    }
}

/// 微信空输入框：`mmui::ChatInputField` 元素 BBox → 「框内首行」锚点。
///
/// 2026-09-21 探针 v7 推翻「UIA 拿不到」的旧结论：该元素级 BoundingRectangle
/// 可信且跟随输入框拖拽高度/屏幕尺寸自适应；空框四路 miss 的真因是 TextPattern
/// selection 为 collapsed range（GetBoundingRectangles 返回空数组），文字级没
/// 矩形、元素级有。锚框顶下移一行（≤32 物理），弹窗底恰好压在框顶附近。
/// b = (left, top, width, height) 物理像素（`bounds_of` 口径）。
fn wechat_field_caret_from_box(b: (f64, f64, f64, f64)) -> Option<(i32, i32)> {
    let (l, t, w, h) = b;
    if w < 120.0 || h < 40.0 {
        return None; // 尺寸不像输入框
    }
    Some((l.round() as i32 + 8, t.round() as i32 + ((h as i32) / 4).min(32)))
}

/// 微信 4.x（mmui）聊天输入区几何估算（UIA 真框拿不到时的兜底）。
///
/// 依据（2026-09-21 UIA 探针，微信 4.1.15.11，DPI 2.0 主窗 1002x679 物理）：
/// - 输入框是 `mmui::ChatInputField`（ControlType.Edit，focus=True），
///   **但无 TextPattern/ValuePattern，且 UIA 包围盒是未裁剪的布局坐标**
///   （报 y=1058 超出物理窗底 750）→ UIA rect 不能当屏幕坐标，只能几何推。
/// - 新版输入框自绘 caret：imm/gui/msaa/uia 四条取插入点的路全 miss（v0.10.8 前的
///   `caret:gui` 路径随 9/17 微信自动更新失效）。
/// - 物理标定：右栏（聊天面板）占水平 33%..100%，输入区占窗底 ~34% 高。
///
/// 独立聊天小窗与主窗同布局，同样适用。
pub fn wechat_input_box(fg: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let (l, t, r, b) = fg;
    let w = (r - l).max(1);
    let h = (b - t).max(1);
    let il = l + w * 36 / 100;
    // clamp 上限 320：2026-09-21 用户点验反馈「弹窗再向上一些」——弹窗底 = it - 12，
    // 上限 260→320 即整体上移 60 物理（30 逻辑），减少对输入框的遮挡。
    let it = b - (h * 34 / 100).clamp(72, 320);
    // 2026-09-21 三次点验：空输入框（四路 miss 且无缓存）呼出时弹窗几乎顶到屏幕
    // 上沿，用户报「位置很奇怪」；输入任意字符后 `caret:uia:hwnd-focus:doc-end`
    // 锚到框内真 caret（y=768），清空后靠 cached 续命也正常。空框的真 caret 初始
    // 就在框内第一行 ≈ 输入区顶 +112 物理（pos_debug 17:24 实测 768 - 656）。
    // 落点下移对齐「框内首行」，与有字符时的 caret 落点一致；小窗至少给框留 80。
    let it = (it + 112).min(b - 80);
    (il, it, r - 8, b - 6)
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

/// Shell 前台（开始菜单/搜索）固定定位时距工作区左上角的留白（WPF `margin`）。
pub const SHELL_MARGIN: i32 = 16;

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
    // 开始菜单/搜索等 Shell 前台：光标的插入点都不属于「要粘东西的目标」，
    // 且 Shell 是居中全屏浮层、Z 序更高，跟光标放必被盖。固定到工作区左上
    // （对齐 WPF `PositionPopupFixedShellWorkArea`），重叠面积最小。
    if is_shell_foreground() {
        if let Some(a) = shell_corner_anchor() {
            remember_anchor(a);
            return (a.0, a.1, "shell-workarea".to_string());
        }
    }
    if mode == "Caret" {
        let exe = fg_exe_name().to_ascii_lowercase();
        // WorkBuddy 定位（2026-09-22 晚定版）：**锚定 UIA 输入框 BBox 居中，不跟光标**。
        // 历史：47c17db 加 field-box 兜底 → 3b909f8 试 caret2 跟光标 → b94d7dc 把 caret2
        // 改成 ±380 对称假框。用户实测「更不好用了，不如输入框上方居中」，且假框中心 =
        // 光标、clamp 又用前台窗跨屏坐标，会导致弹窗横飘 + 跨屏错屏（pos_debug 实锤
        // anchor=(2875,884) 算出 rect 甩到主屏右端）。现回到「居中于输入框」语义。
        // 残余不稳项（UIA 底栏常判 no-composer、对话小窗/首页偶贴地）见 ROADMAP 欠账。
        if exe.contains("workbuddy") {
            if let Some(fg) = fg_rect() {
                // prefer_caret=false：跳过 TextPattern2 的 caret 假框，只用 parent/field-box
                // 链拿真实输入框 BBox（拿不到再几何估）。见 `uia_composer_box` 注释。
                let (found, why) = wait_uia_composer_box(150, fg, false);
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
            let exe = fg_exe_name().to_ascii_lowercase();
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
        // 微信 4.1.15+（2026-09-17 自动更新）：mmui 输入框自绘 caret 且无 TextPattern，
        // imm/gui/msaa/uia 四条取插入点的路全 miss（pos_debug.log 实测），不能在这里
        // 直接回退鼠标。聊天输入区固定在窗口底部（wechat_input_box 有标定依据），
        // 几何锚定输入区、弹窗放其上方 —— 与用户意图（往输入框粘东西）一致。
        if is_wechat_fg() {
            if let Some(fg) = fg_rect() {
                let box_rc = wechat_input_box(fg);
                if let Ok(mut guard) = INPUT_BOX.lock() {
                    *guard = Some(box_rc);
                }
                if let Ok(mut guard) = HOST_FG.lock() {
                    *guard = Some(fg);
                }
                PLACE_ON_BOX.store(true, Ordering::Relaxed);
                remember_anchor((box_rc.0, box_rc.1));
                return (box_rc.0, box_rc.1, "wechat-input-box".to_string());
            }
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
/// 无条件落盘（不受 `CLIPX_DEBUG` 门控）。迁移收尾这类一次性低频动作必须
/// 恒留痕——出问题时用户不会去设调试变量，事后也无从复现。
pub fn write_data_log(file: &str, line: &str) {
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

/// 非 Windows 存根：`write_data_log` 的 Windows 版写 exe 旁 `Data/`，
/// 而迁移收尾（`legacy_wpf`）本身只在 Windows 存在，这里保持同名空实现即可。
///
/// **不能省**：省了会让 `legacy_wpf::log()`（跨平台 `pub fn`）在 ubuntu/macos 的
/// test job 上报 `E0425: cannot find function write_data_log` —— 2026-09-22 就红了两轮
/// （本机 `cargo check` 是 Windows target，看不见）。同目录的 `append_debug_log`/
/// `resolve_popup_anchor`/`fg_debug` 都早有 `not(windows)` 存根，这个当初漏了。
#[cfg(not(windows))]
pub fn write_data_log(_file: &str, _line: &str) {}

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

/// 前台窗口所属进程的 exe **文件名**（含 `.exe` 后缀，如 "Weixin.exe"）。
/// 名字曾叫 `fg_exe_stem`——但实现从没去掉过后缀，2026-09-21 的微信特判
/// 就是被这个误导按「stem」精确比较 `== "weixin"` 坑死的，故改名正名。
#[cfg(windows)]
fn fg_exe_name() -> String {
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

/// 前台是否微信**主进程**（Weixin.exe / 旧版 WeChat.exe）。
/// 不用 `is_wechat_token`：它会把 WeChatAppEx.exe（小程序/内置浏览器）也算进来，
/// 而那类窗口输入框不固定在底部，几何猜输入区会错位。
/// exe 文件名是否微信**主进程**（Weixin.exe / 旧版 WeChat.exe）。
/// 后缀归一化后精确比较，不用 token 匹配：`WeChatAppEx.exe`（小程序/内置浏览器）
/// 的输入框不固定在底部，几何猜输入区会错位，必须排除。
fn exe_matches_wechat(name: &str) -> bool {
    let exe = name.to_ascii_lowercase();
    let stem = exe.strip_suffix(".exe").unwrap_or(exe.as_str());
    stem == "weixin" || stem == "wechat"
}

#[cfg(windows)]
fn is_wechat_fg() -> bool {
    exe_matches_wechat(&fg_exe_name())
}

/// 前台是否开始菜单/搜索等 Shell 层（WPF `IsShellForegroundWindow`）。
///
/// 这些进程的窗口是「全屏浮层 + 居中面板」，我们的弹窗若跟光标/输入框放置会被
/// 它盖住（Win11 下 Shell 处于更高 Z 带，用户态 TOPMOST 也压不过）。检测到就
/// 退到工作区左上角固定位（对齐 WPF `PositionPopupFixedShellWorkArea`），
/// 把重叠面积降到最低。
///
/// 专门宿主进程直接命中；explorer.exe 需再看类名 —— 只认 WinUI CoreWindow
/// （任务栏搜索等），**不能**把 CabinetWClass/ExploreWClass 文件窗口误判成 Shell。
#[cfg(windows)]
pub(crate) fn is_shell_foreground() -> bool {
    if SHELL_DEMO_FORCE.load(Ordering::SeqCst) {
        return true;
    }
    let exe = fg_exe_name().to_ascii_lowercase();
    let stem = exe.strip_suffix(".exe").unwrap_or(exe.as_str());
    if is_dedicated_shell_host(stem) {
        return true;
    }
    if stem == "explorer" {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let fg = unsafe { GetForegroundWindow() };
        return hwnd_class_name(fg).eq_ignore_ascii_case("Windows.UI.Core.CoreWindow");
    }
    false
}

/// 纯逻辑：Shell 宿主进程名（不含 .exe）。可单测。
fn is_dedicated_shell_host(stem: &str) -> bool {
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "searchhost" | "startmenuexperiencehost" | "shellexperiencehost" | "shellhost"
    )
}

/// Shell 前台时的固定锚点：当前显示器工作区左上 + 16px（WPF margin）。
/// 取光标所在屏；无光标信息则退回主屏工作区原点。
#[cfg(windows)]
fn shell_corner_anchor() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let (cx, cy) = unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            (pt.x, pt.y)
        } else {
            (0, 0)
        }
    };
    let (work, _) = monitor_work_and_dpi(cx, cy);
    Some((work.0 + SHELL_MARGIN, work.1 + SHELL_MARGIN))
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
///
/// `prefer_caret`：true 时优先用 TextPattern2 的 caret 位置（跟光标走，适合
/// 微信/Office 这类输入框本身不稳定的宿主）；false 时跳过 caret，只用焦点
/// 输入控件的 BBox（WorkBuddy 用，保证「输入框上方居中」不随光标横抖）。
#[cfg(windows)]
fn wait_uia_composer_box(
    ms: u64,
    fg: (i32, i32, i32, i32),
    prefer_caret: bool,
) -> (Option<(i32, i32, i32, i32)>, String) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let _ = std::thread::Builder::new()
        .name("clipx-uia-box".into())
        .spawn(move || {
            let _ = tx.send(uia_composer_box(fg, prefer_caret));
        });
    rx.recv_timeout(std::time::Duration::from_millis(ms))
        .unwrap_or((None, "timeout".to_string()))
}

#[cfg(windows)]
fn uia_composer_box(
    fg: (i32, i32, i32, i32),
    prefer_caret: bool,
) -> (Option<(i32, i32, i32, i32)>, String) {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayUnaccessData};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
        IUIAutomationTextPattern2, TextUnit_Character, TreeScope_Descendants,
        UIA_HasKeyboardFocusPropertyId, UIA_TextPattern2Id, UIA_TextPatternId,
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
        // TextPattern2 GetCaretRange（优先级最高 = 跟光标走）：
        // GetSelection 只反映「选中文字」，无选区时恒 collapsed 且 rects 空
        // （v30/v31 实锤，与输入框有无内容无关）；真光标在 GetCaretRange 的
        // zero-length range 里，collapsed 直取矩形为空时先扩到一个字符再取
        // （2026-09-22 v34 实锤 Chromium 140 的 Edit 支持 TextPattern2，QI 可用）。
        // miss 原因带进 branch 串（pos_debug 单行可见），用于命中率校准。
        let mut caret2_miss = if prefer_caret {
            "no-tp2".to_string()
        } else {
            "skipped(host-wants-box)".to_string()
        };
        if prefer_caret {
            if let Ok(tp2) = el.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) {
                caret2_miss = "no-range".to_string();
                let mut active = Default::default();
                if let Ok(range) = tp2.GetCaretRange(&mut active) {
                    caret2_miss = "no-rect".to_string();
                    let mut rect = text_range_first_rect(&range);
                    if rect.is_none() && range.ExpandToEnclosingUnit(TextUnit_Character).is_ok() {
                        rect = text_range_first_rect(&range);
                    }
                    if let Some((rx, ry, rw, rh)) = rect {
                        if rw < 48 {
                            // caret 矩形是窄条；过宽的不是光标（选区/整行）
                            let pt = (rx, ry + rh); // 光标底部 = 插入行基线
                            if point_in_fg(pt.0, pt.1, fg) {
                                // 弹窗水平中心对准光标（对称插入框）——非对称 760 框会把
                                // 弹窗整体推向光标右侧 154px，视觉上「没跟着挪」。
                                let bx0 = (pt.0 - 380).max(fg.0 + 8);
                                let bx1 = (pt.0 + 380).min(fg.2 - 8);
                                let by0 = (pt.1 - 16).max(fg.1 + 24);
                                let by1 = (pt.1 + 112).min(fg.3 - 12);
                                return done(
                                    Some((bx0, by0, bx1.max(bx0 + 80), by1.max(by0 + 48))),
                                    "caret2".to_string(),
                                );
                            }
                            caret2_miss = format!("out-of-fg({},{})", pt.0, pt.1);
                        } else {
                            caret2_miss = format!("too-wide({}x{})", rw, rh);
                        }
                    }
                }
            }
        }
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
                    "parent ({},{},{}x{})|c2={}",
                    b.0,
                    b.1,
                    b.2 - b.0,
                    b.3 - b.1,
                    caret2_miss
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
        // TextPattern selection 是 collapsed range 时 GetBoundingRectangles
        // 返回空数组（2026-09-22 v30 探针实锤：WorkBuddy 空输入框 selections=1
        // 但 rects=[]，与微信 4.x 空框同构）——上面 focused-caret 分支因此
        // 拿不到点。此时元素级 BBox 仍可信：空框也给、跟随窗口宽度/布局
        // 自适应（v27 实锤 762↔778 随窗宽变化），直接作定位框。
        if let Some(b) = bounds(&el) {
            if is_usable_composer(Some(b), fg) {
                let bb = box_of(b);
                return done(
                    Some(bb),
                    format!(
                        "field-box ({},{},{}x{})",
                        bb.0,
                        bb.1,
                        bb.2 - bb.0,
                        bb.3 - bb.1
                    ),
                );
            }
        }
        done(None, format!("no-composer|c2={caret2_miss}"))
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
            // 微信 4.x 空输入框：ChatInputField 带焦点但 TextPattern selection 是
            // collapsed range（GetBoundingRectangles 返回空数组），文字级拿不到 →
            // 元素级 BBox 兜底当「框内首行」锚点（跟随拖拽高度/屏幕尺寸自适应）。
            if wechat && class_of(el) == "mmui::ChatInputField" {
                if let Some(b) = bounds_of(el) {
                    if let Some(pt) = wechat_field_caret_from_box(b) {
                        if caret_point_usable(pt.0, pt.1, fg_rc, true) {
                            if co.is_ok() {
                                CoUninitialize();
                            }
                            return (Some(pt), "uia:field-box".to_string());
                        }
                    }
                }
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
        // Shell（开始/搜索）前台时：Win11 把 Shell 放在更高 Z 带，单纯 HWND_TOPMOST
        // 也会被盖。先置 TOPMOST，再把本窗插到 Shell 根窗口之上
        // （对齐 WPF `ApplyShellForegroundZOrderFix`），尽最大努力露出来。
        let insert_after = shell_insert_after(hwnd);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            pw.max(1),
            ph.max(1),
            SWP_NOACTIVATE,
        );
        if let Some(after) = insert_after {
            let _ = SetWindowPos(
                hwnd,
                Some(after),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
    resize_hook::end_our_pos();
}

/// Shell 前台时返回其**根窗口**句柄（用于把弹窗插到它之上）；
/// 非 Shell 前台或取不到时返回 None。对齐 WPF `GetAncestor(fg, GA_ROOT)`。
#[cfg(windows)]
fn shell_insert_after(me: windows::Win32::Foundation::HWND) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetForegroundWindow, GA_ROOT};
    if !is_shell_foreground() {
        return None;
    }
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == me {
            return None;
        }
        let root = GetAncestor(fg, GA_ROOT);
        let after = if root.0.is_null() { fg } else { root };
        if after == me {
            None
        } else {
            Some(after)
        }
    }
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
        let fg = (0, 0, 1920, 1080);
        assert!(caret_point_usable(480, 920, Some(fg), true));
    }

    #[test]
    fn wechat_input_box_geometry() {
        // 主窗 1002x679（探针标定用的物理尺寸）：输入区应在右下、顶部约在 66% 高处
        let fg = (19, 71, 1021, 750);
        let (il, it, ir, ib) = wechat_input_box(fg);
        assert!(il > fg.0 + (fg.2 - fg.0) / 3, "左边界应越过左侧栏: {il}");
        assert_eq!(ir, fg.2 - 8);
        assert_eq!(ib, fg.1 + (fg.3 - fg.1) - 6);
        let h = fg.3 - fg.1;
        assert_eq!(it, fg.3 - (h * 34 / 100).clamp(72, 320) + 112);
        assert!(ib > it);
        // 超高窗口：输入区高度封顶 320（再 +112 对齐框内首行）
        let tall = (0, 0, 1600, 1200);
        let (_, it2, _, ib2) = wechat_input_box(tall);
        assert_eq!(it2, tall.3 - 320 + 112);
        assert!(ib2 > it2);
        // 小窗：+112 会被 b-80 封住，框不越窗底
        let tiny = (0, 0, 800, 300);
        let (_, it3, _, ib3) = wechat_input_box(tiny);
        assert_eq!(it3, tiny.3 - 80);
        assert!(ib3 > it3);
        assert!(ib2 > it2);
    }

    #[test]
    fn caret_in_fg_anywhere_ok() {
        let fg = (-1920, 0, 0, 1080);
        assert!(caret_point_usable(-1600, 80, Some(fg), true));
        assert!(caret_point_usable(-800, 900, Some(fg), true));
    }

    #[test]
    fn exe_matches_wechat_names() {
        // 2026-09-21 事故回归：fg_exe_name 返回的是含 .exe 的完整文件名，
        // 曾按 "stem" 精确比较 == "weixin" 恒 false，微信特判整条失效。
        assert!(exe_matches_wechat("Weixin.exe"));
        assert!(exe_matches_wechat("weixin.exe"));
        assert!(exe_matches_wechat("WeChat.exe"));
        assert!(exe_matches_wechat("Weixin")); // 无后缀防御
        assert!(exe_matches_wechat("wechat"));
        assert!(!exe_matches_wechat("WeChatAppEx.exe")); // 小程序/内置浏览器不算
        assert!(!exe_matches_wechat("WXWork.exe")); // 企业微信走 caret:gui，无关
        assert!(!exe_matches_wechat("explorer.exe"));
        assert!(!exe_matches_wechat(""));
    }

    #[test]
    fn wechat_field_caret_from_box_checks() {
        // 正常输入框：锚在框内左上、下移一行（≤32 物理）
        let pt = wechat_field_caret_from_box((3750.0, 1218.0, 1154.0, 578.0)).unwrap();
        assert_eq!(pt, (3758, 1250));
        // 拖大的框也只下一行，不会扎进框中央
        let tall = wechat_field_caret_from_box((0.0, 0.0, 1900.0, 1200.0)).unwrap();
        assert_eq!(tall, (8, 32));
        // 不像输入框的尺寸必须拒
        assert!(wechat_field_caret_from_box((0.0, 0.0, 100.0, 30.0)).is_none());
        assert!(wechat_field_caret_from_box((0.0, 0.0, 50.0, 800.0)).is_none());
    }

    #[test]
    fn shell_host_process_names_detected() {
        // Win11 开始菜单/搜索的专用宿主
        assert!(is_dedicated_shell_host("StartMenuExperienceHost"));
        assert!(is_dedicated_shell_host("SearchHost"));
        assert!(is_dedicated_shell_host("ShellExperienceHost"));
        assert!(is_dedicated_shell_host("ShellHost"));
        assert!(is_dedicated_shell_host("searchhost")); // 大小写不敏感
        // explorer / 文件窗口 / 普通应用不能算 Shell
        assert!(!is_dedicated_shell_host("explorer"));
        assert!(!is_dedicated_shell_host("WorkBuddy"));
        assert!(!is_dedicated_shell_host(""));
    }

    #[test]
    fn shell_corner_anchor_offsets_by_margin() {
        // 纯逻辑校验：工作区左上 + 16，且必落在工作区内
        let work = (1920, 0, 3840, 1080);
        let a = (work.0 + SHELL_MARGIN, work.1 + SHELL_MARGIN);
        assert_eq!(a, (1936, 16));
        assert!(a.0 >= work.0 && a.1 >= work.1);
        assert!(a.0 < work.2 && a.1 < work.3);
    }

    #[test]
    fn cross_monitor_box_clamps_to_box_monitor() {
        // 跨屏回归：锚点判屏在副屏，但输入框 BBox 其实在主屏时，
        // popup_rect_on_box 必须按传入 work（=box 所在屏）夹紧，不越界。
        let box_rc = (100, 800, 900, 1000); // 主屏输入框
        let work = (0, 0, 1920, 1040); // 主屏工作区
        let (x, y, pw, ph) = popup_rect_on_box(box_rc, 452.0, 592.0, 1.0, 1.0, work, None);
        assert!(x >= work.0 && x + pw <= work.2, "x={x} pw={pw}");
        assert!(y >= work.1 && y + ph <= work.3, "y={y} ph={ph}");
        // 水平居中于输入框
        let mid = (box_rc.0 + box_rc.2) / 2;
        assert!((x + pw / 2 - mid).abs() < 2, "x={x} mid={mid}");
    }
}
