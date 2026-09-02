//! 逻辑线程：唯一持有弹窗状态（查询/过滤/列表/选中项/预览/可见性），
//! 所有输入源（键盘钩子/鼠标钩子/热键/托盘/处理器/OCR）经 AppEvt 汇入此线程，
//! UI 更新统一经 invoke_from_event_loop 回主线程（channel 模式，全平台约定）。

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use clipboard_rs::ClipboardContext;
use clipx_core::{now_ms, time::time_ago, ClipboardGate, EntryKind, EntryMeta};
use clipx_store::Store;
use slint::{ComponentHandle, LogicalSize, ModelRc, SharedString, VecModel, WindowSize};

use crate::keyboard_hook::{self, KeyEvt};
use crate::settings::Settings;
use crate::{mouse_hook, paste, win_popup, PopupWindow, RowData};

pub const LIST_LIMIT: i64 = 2000;
const WIN_W: f32 = 420.0;
const WIN_MAX_H: f32 = 560.0;
const WIN_MIN_H: f32 = 200.0;
const HEADER_H: f32 = 44.0;
const SEARCH_H: f32 = 48.0;
const FOOTER_H: f32 = 30.0;
const ROW_H: f32 = 44.0;
const QUERY_MAX_CHARS: usize = 64;

/// 预览大图解码上限（长边）：4K 图先等比缩小再进 UI，
/// 控制软件渲染下的解码内存峰值（原图 bytes 随即释放）。
const PREVIEW_MAX_DIM: u32 = 1600;

/// 跨线程像素载荷：slint::Image 非 Send，逻辑线程只产原始 RGBA，
/// SharedPixelBuffer/Image 在事件循环线程上组装。
#[derive(Clone, Default)]
struct ImageData {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

#[derive(Debug)]
pub enum AppEvt {
    Key(KeyEvt),
    Toggle,
    Hide,
    ListChanged,
    /// OCR 完成（或新图片入队后处理完）：刷新列表与预览中的 OCR 文本
    OcrDone,
    RowClicked(i32),
    RowDoubleClicked(i32),
    FilterCycle,
    /// 右键某行 / Menu 键：打开上下文菜单
    MenuRequest(i32),
    /// 菜单动作执行
    MenuAction(MenuAction),
    /// 点击菜单外关闭
    MenuClose,
}

/// 右键上下文菜单动作
#[derive(Debug, Clone, Copy)]
pub enum MenuAction {
    Copy,
    Pin,
    Delete,
}

pub struct LogicDeps {
    pub store: Store,
    pub settings: Settings,
    pub gate: ClipboardGate,
}

struct State {
    query: String,
    filter: Option<EntryKind>,
    items: Vec<EntryMeta>,
    selected: usize,
    visible: bool,
    preview_open: bool,
    preview: Option<PreviewData>,
    /// entry_id → 行内缩略图像素（仅解码一次；图片条数受 max_image_items 上限约束）
    thumb_cache: HashMap<i64, ImageData>,
    /// 弹窗隐藏时刻（空闲 trim 的计时锚点）
    hidden_at: Option<std::time::Instant>,
    /// 右键上下文菜单：是否打开 / 作用于哪一行
    menu_open: bool,
    menu_index: i32,
    /// Menu 键路径：本次 push 需在 Slint 侧按索引计算菜单位置（右键路径不需要）
    menu_keyboard_pending: bool,
    #[cfg(windows)]
    foreground_at_show: isize,
}

impl State {
    fn new() -> Self {
        Self {
            query: String::new(),
            filter: None,
            items: Vec::new(),
            selected: 0,
            visible: false,
            preview_open: false,
            preview: None,
            thumb_cache: HashMap::new(),
            // 启动即进入空闲计时：弹窗从未弹出的会话（迁移 OCR 回填突发后）
            // 也要周期性 trim，避免分配器滞留的工作集虚高
            hidden_at: Some(std::time::Instant::now()),
            menu_open: false,
            menu_index: -1,
            menu_keyboard_pending: false,
            #[cfg(windows)]
            foreground_at_show: 0,
        }
    }
}

struct PreviewData {
    has_image: bool,
    image: ImageData,
    text: String,
    info: String,
}

pub fn spawn(
    deps: LogicDeps,
    evt_rx: Receiver<AppEvt>,
    weak: slint::Weak<PopupWindow>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("clipx-logic".into())
        .spawn(move || {
            let mut state = State::new();
            let clipboard = ClipboardContext::new().ok();
            loop {
                match evt_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(evt) => handle(evt, &mut state, &deps, &weak, clipboard.as_ref()),
                    Err(RecvTimeoutError::Timeout) => check_foreground(&mut state, &weak),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("启动逻辑线程失败: {e}"))
}

fn handle(
    evt: AppEvt,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    match evt {
        AppEvt::Toggle => {
            if state.visible {
                hide_popup(state, weak);
            } else {
                show_popup(state, deps, weak);
            }
        }
        AppEvt::Hide => {
            if state.visible {
                hide_popup(state, weak);
            }
        }
        AppEvt::ListChanged => {
            if state.visible {
                refresh_thumbs(state, deps);
                refresh(state, deps, weak, false);
            }
        }
        AppEvt::OcrDone => {
            if state.visible {
                reload_preview_if_open(state, deps);
                refresh(state, deps, weak, false);
            }
        }
        AppEvt::RowClicked(i) => {
            if state.visible {
                state.selected = (i.max(0) as usize).min(state.items.len().saturating_sub(1));
                reload_preview_if_open(state, deps);
                push_ui(state, weak);
            }
        }
        AppEvt::RowDoubleClicked(i) => {
            if state.visible {
                do_paste(state, deps, weak, clipboard, i.max(0) as usize);
            }
        }
        AppEvt::FilterCycle => {
            if state.visible {
                state.filter = cycle_filter(state.filter);
                refresh(state, deps, weak, true);
            }
        }
        AppEvt::MenuRequest(i) => {
            if state.visible {
                let idx = (i.max(0) as usize).min(state.items.len().saturating_sub(1));
                state.selected = idx;
                state.menu_open = true;
                state.menu_index = idx as i32;
                state.menu_keyboard_pending = false;
                reload_preview_if_open(state, deps);
                push_ui(state, weak);
            }
        }
        AppEvt::MenuClose => {
            if state.menu_open {
                state.menu_open = false;
                push_ui(state, weak);
            }
        }
        AppEvt::MenuAction(action) => {
            if state.visible {
                menu_action(action, state, deps, weak, clipboard);
            }
        }
        AppEvt::Key(k) => {
            if state.visible {
                handle_key(k, state, deps, weak, clipboard);
            }
        }
    }
}

fn handle_key(
    k: KeyEvt,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    // 菜单打开期间：任意键（含 Esc）关闭菜单并吞掉，不穿透到背后列表
    // （菜单目标行可能与键盘选中行错位，穿透操作会作用在错误条目上）
    if state.menu_open {
        state.menu_open = false;
        push_ui(state, weak);
        return;
    }
    match k {
        KeyEvt::Esc => hide_popup(state, weak),
        KeyEvt::Enter => do_paste(state, deps, weak, clipboard, state.selected),
        KeyEvt::Space => toggle_preview(state, deps, weak),
        KeyEvt::PinToggle => {
            if let Some(meta) = state.items.get(state.selected).cloned() {
                let _ = deps.store.toggle_pin(meta.id);
                // 置顶条目浮动到顶部：选中跟随原条目而非原索引
                refresh(state, deps, weak, false);
                state.selected = state
                    .items
                    .iter()
                    .position(|m| m.id == meta.id)
                    .unwrap_or(state.selected);
                reload_preview_if_open(state, deps);
                push_ui(state, weak);
            }
        }
        KeyEvt::Menu => {
            if !state.items.is_empty() {
                state.menu_open = true;
                state.menu_index = state.selected as i32;
                state.menu_keyboard_pending = true;
                push_ui(state, weak);
            }
        }
        KeyEvt::Char(c) => {
            if state.query.chars().count() < QUERY_MAX_CHARS {
                state.query.push(c);
                refresh(state, deps, weak, true);
            }
        }
        KeyEvt::Digit(n) => {
            if state.query.is_empty() && (n as usize) <= state.items.len() && n >= 1 {
                do_paste(state, deps, weak, clipboard, (n - 1) as usize);
            } else {
                handle_key(
                    KeyEvt::Char((b'0' + n) as char),
                    state,
                    deps,
                    weak,
                    clipboard,
                );
            }
        }
        KeyEvt::Backspace => {
            if state.query.pop().is_some() {
                refresh(state, deps, weak, true);
            }
        }
        KeyEvt::Delete => {
            if let Some(meta) = state.items.get(state.selected).cloned() {
                deps.store.delete(meta.id);
                refresh_thumbs(state, deps);
                refresh(state, deps, weak, false);
                reload_preview_if_open(state, deps);
            }
        }
        KeyEvt::Up => move_selection(state, deps, weak, -1),
        KeyEvt::Down => move_selection(state, deps, weak, 1),
        KeyEvt::PgUp => move_selection(state, deps, weak, -10),
        KeyEvt::PgDn => move_selection(state, deps, weak, 10),
        KeyEvt::Home => {
            state.selected = 0;
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
        KeyEvt::End => {
            state.selected = state.items.len().saturating_sub(1);
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
    }
}

fn move_selection(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    delta: i32,
) {
    let len = state.items.len();
    if len == 0 {
        return;
    }
    let cur = state.selected as i64;
    let next = (cur + delta as i64).clamp(0, len as i64 - 1);
    state.selected = next as usize;
    reload_preview_if_open(state, deps);
    push_ui(state, weak);
}

/// Space 切换预览（WPF 版行为）：打开时加载当前选中项，
/// 切换选中项后预览内容跟随。
fn toggle_preview(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    if state.preview_open {
        state.preview_open = false;
        state.preview = None;
    } else {
        if state.items.is_empty() {
            return;
        }
        state.preview = Some(load_preview(&state.items[state.selected], &deps.store));
        state.preview_open = true;
    }
    push_ui(state, weak);
}

fn reload_preview_if_open(state: &mut State, deps: &LogicDeps) {
    if state.preview_open {
        state.preview = state
            .items
            .get(state.selected)
            .map(|m| load_preview(m, &deps.store));
    }
}

fn load_preview(meta: &EntryMeta, store: &Store) -> PreviewData {
    match meta.kind {
        EntryKind::Text => {
            let full = store.get_text(meta.id).unwrap_or_default();
            let n = full.chars().count();
            PreviewData {
                has_image: false,
                image: ImageData::default(),
                text: full,
                info: format!("文本 · {n} 字"),
            }
        }
        EntryKind::Image => {
            let ocr = store.get_ocr(meta.id);
            let ocr_state = ocr.as_ref().map(|o| o.state).unwrap_or(0);
            let ocr_text = ocr.and_then(|o| o.text).unwrap_or_default();
            let (image, info) = match store.get_image(meta.id) {
                Some(row) => {
                    // 懒加载 + 限尺寸解码；row（原图 bytes）在本函数结束即释放
                    let img = decode_image_limited(&row.blob, PREVIEW_MAX_DIM);
                    (img, format!("图片 {}×{}", row.w, row.h))
                }
                None => (ImageData::default(), "图片".to_string()),
            };
            let info = match ocr_state {
                2 if ocr_text.trim().is_empty() => format!("{info} · OCR：未识别到文字"),
                2 => format!("{info} · OCR 文本"),
                3 => format!("{info} · OCR 失败"),
                _ => format!("{info} · OCR 进行中…"),
            };
            PreviewData {
                has_image: true,
                image,
                text: ocr_text,
                info,
            }
        }
        EntryKind::Files => {
            let paths = store.get_text(meta.id).unwrap_or_default();
            let n = paths.lines().count();
            PreviewData {
                has_image: false,
                image: ImageData::default(),
                text: paths,
                info: format!("文件 · {n} 项"),
            }
        }
        EntryKind::RichText => {
            let full = store.get_text(meta.id).unwrap_or_default();
            let n = full.chars().count();
            PreviewData {
                has_image: false,
                image: ImageData::default(),
                text: full,
                info: format!("富文本 · {n} 字 · 粘贴还原格式"),
            }
        }
    }
}

/// PNG bytes → 原始 RGBA（长边超限时等比缩小，解码内存上限可控）。
fn decode_image_limited(png: &[u8], max_dim: u32) -> ImageData {
    let Ok(img) = image::load_from_memory(png) else {
        return ImageData::default();
    };
    let img = if img.width().max(img.height()) > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    ImageData {
        rgba: rgba.into_raw(),
        w,
        h,
    }
}

/// 事件循环线程上调用：RGBA → slint::Image。
fn to_slint_image(d: ImageData) -> slint::Image {
    if d.w == 0 || d.h == 0 {
        return slint::Image::default();
    }
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(d.w, d.h);
    let dst = buf.make_mut_slice();
    for (o, s) in dst.iter_mut().zip(d.rgba.chunks_exact(4)) {
        *o = slint::Rgba8Pixel {
            r: s[0],
            g: s[1],
            b: s[2],
            a: s[3],
        };
    }
    slint::Image::from_rgba8(buf)
}

/// 增量刷新行内缩略图缓存：一次批量查询，仅解码缓存未命中的条目。
fn refresh_thumbs(state: &mut State, deps: &LogicDeps) {
    for (id, t) in deps.store.list_thumbs(500) {
        state
            .thumb_cache
            .entry(id)
            .or_insert_with(|| decode_image_limited(&t.blob, 256));
    }
}

fn show_popup(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    state.query.clear();
    state.filter = None;
    state.preview_open = false;
    state.preview = None;
    state.menu_open = false;
    state.menu_index = -1;
    state.hidden_at = None;
    refresh_thumbs(state, deps);
    state.items = deps.store.search("", None, LIST_LIMIT);
    state.selected = 0;
    state.visible = true;
    keyboard_hook::set_visible(true);
    #[cfg(windows)]
    {
        state.foreground_at_show = win_popup::foreground_hwnd();
    }
    let mut bundle = ui_bundle(state);
    bundle.show = true;
    invoke_ui(weak, bundle);
}

fn hide_popup(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    state.visible = false;
    state.preview_open = false;
    state.preview = None;
    state.menu_open = false;
    state.menu_index = -1;
    state.hidden_at = Some(std::time::Instant::now());
    keyboard_hook::set_visible(false);
    #[cfg(windows)]
    mouse_hook::POPUP_HWND.store(0, std::sync::atomic::Ordering::SeqCst);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().hide();
        }
    });
}

fn refresh(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    reset_selection: bool,
) {
    state.items = deps.store.search(&state.query, state.filter, LIST_LIMIT);
    if reset_selection {
        state.selected = 0;
    } else {
        state.selected = state.selected.min(state.items.len().saturating_sub(1));
    }
    push_ui(state, weak);
}

/// 按类型回写剪贴板（Gate 防自采在 arm 后由 monitor 吸收）。
/// 返回 false = 条目数据缺失（图片/文件读库失败），调用方不应继续。
fn write_entry_clipboard(
    meta: &EntryMeta,
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
) -> bool {
    let Some(ctx) = clipboard else { return false };
    match meta.kind {
        EntryKind::Image => {
            let Some(img) = deps.store.get_image(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_image(ctx, &img.blob).is_ok()
        }
        EntryKind::Files => {
            let Some(paths) = deps.store.get_files(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_files(ctx, &paths).is_ok()
        }
        EntryKind::RichText => {
            // 投影 + HTML 同时写回；html 缺失时退化为纯文本
            if let Some(html) = deps.store.get_html(meta.id) {
                let text = deps.store.get_text(meta.id).unwrap_or_default();
                deps.gate.arm();
                return paste::write_rich_text(ctx, &text, &html).is_ok();
            }
            let Some(text) = deps.store.get_text(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_text(ctx, &text).is_ok()
        }
        EntryKind::Text => {
            let Some(text) = deps.store.get_text(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_text(ctx, &text).is_ok()
        }
    }
}

fn do_paste(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    let Some(meta) = state.items.get(idx) else {
        return;
    };
    if !write_entry_clipboard(meta, deps, clipboard) {
        return;
    }

    hide_popup(state, weak);
    if deps.settings.paste_simulate {
        std::thread::sleep(Duration::from_millis(80));
        paste::send_ctrl_v();
    }
}

/// 右键菜单动作（menu_index 行；菜单已在 UI 侧关闭，这里只管状态与数据）
fn menu_action(
    action: MenuAction,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    let idx = state.menu_index.max(0) as usize;
    state.menu_open = false;
    let Some(meta) = state.items.get(idx).cloned() else {
        push_ui(state, weak);
        return;
    };
    match action {
        // 复制：只写剪贴板（不模拟 Ctrl+V），写完收起弹窗
        MenuAction::Copy => {
            if write_entry_clipboard(&meta, deps, clipboard) {
                hide_popup(state, weak);
            } else {
                push_ui(state, weak);
            }
        }
        MenuAction::Pin => {
            let _ = deps.store.toggle_pin(meta.id);
            refresh(state, deps, weak, false);
            // 置顶浮动后选中跟随原条目
            state.selected = state
                .items
                .iter()
                .position(|m| m.id == meta.id)
                .unwrap_or(state.selected);
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
        MenuAction::Delete => {
            deps.store.delete(meta.id);
            refresh_thumbs(state, deps);
            refresh(state, deps, weak, false);
            reload_preview_if_open(state, deps);
        }
    }
}

fn check_foreground(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    // 空闲 trim：隐藏满 5 秒归还一次工作集，之后每满 5 秒重复
    // （burst/预览等峰值操作后 WS 不虚高；trim 后 soft fault 廉价拉回）
    if !state.visible {
        let idle_5s = state
            .hidden_at
            .map(|t| t.elapsed() >= Duration::from_secs(5))
            .unwrap_or(false);
        if idle_5s {
            win_popup::trim_working_set();
            state.hidden_at = Some(std::time::Instant::now());
        }
    }
    #[cfg(windows)]
    {
        if state.visible && state.foreground_at_show != 0 {
            let fg = win_popup::foreground_hwnd();
            if fg != state.foreground_at_show {
                hide_popup(state, weak);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = weak;
    }
}

/// 逻辑线程产出的行数据（纯数据，Send）；RowData（含 slint::Image）
/// 在事件循环线程上组装。
struct RowSource {
    id: i32,
    kind: i32,
    icon: &'static str,
    index_label: String,
    preview: String,
    sub: String,
    time_ago: String,
    thumb: ImageData,
}

struct UiBundle {
    rows: Vec<RowSource>,
    selected: i32,
    search_active: bool,
    search_text: SharedString,
    search_count: SharedString,
    filter_label: &'static str,
    item_count: SharedString,
    height: f32,
    show: bool,
    preview_active: bool,
    preview_has_image: bool,
    preview_image: ImageData,
    preview_text: SharedString,
    preview_info: SharedString,
    menu_visible: bool,
    menu_index: i32,
    menu_pinned: bool,
    menu_position_keyboard: bool,
}

fn ui_bundle(state: &mut State) -> UiBundle {
    let (preview_active, preview_has_image, preview_image, preview_text, preview_info) =
        match state.preview.as_ref() {
            Some(p) => (
                state.preview_open,
                p.has_image,
                p.image.clone(),
                p.text.clone().into(),
                p.info.clone().into(),
            ),
            None => (
                false,
                false,
                ImageData::default(),
                SharedString::new(),
                SharedString::new(),
            ),
        };
    let menu_index = if state.menu_open {
        state.menu_index
    } else {
        -1
    };
    let bundle = UiBundle {
        rows: build_rows(&state.items, &state.thumb_cache),
        selected: state.selected as i32,
        search_active: !state.query.is_empty(),
        search_text: state.query.clone().into(),
        search_count: format!("{} 条结果", state.items.len()).into(),
        filter_label: filter_label(state.filter),
        item_count: format!("{} 条", state.items.len()).into(),
        height: window_height(state.items.len(), !state.query.is_empty()),
        show: false,
        preview_active,
        preview_has_image,
        preview_image,
        preview_text,
        preview_info,
        menu_visible: state.menu_open,
        menu_index,
        menu_pinned: state
            .items
            .get(menu_index.max(0) as usize)
            .map(|m| m.pinned)
            .unwrap_or(false),
        menu_position_keyboard: state.menu_keyboard_pending,
    };
    state.menu_keyboard_pending = false;
    bundle
}

fn push_ui(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    invoke_ui(weak, ui_bundle(state));
}

fn invoke_ui(weak: &slint::Weak<PopupWindow>, bundle: UiBundle) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let rows: Vec<RowData> = bundle
            .rows
            .into_iter()
            .map(|r| RowData {
                id: r.id,
                kind: r.kind,
                icon: r.icon.into(),
                index_label: r.index_label.into(),
                preview: r.preview.into(),
                sub: r.sub.into(),
                time_ago: r.time_ago.into(),
                thumb: to_slint_image(r.thumb),
            })
            .collect();
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_selected_index(bundle.selected);
        ui.set_search_active(bundle.search_active);
        ui.set_search_text(bundle.search_text);
        ui.set_search_count(bundle.search_count);
        ui.set_type_filter_label(bundle.filter_label.into());
        ui.set_item_count(bundle.item_count);
        ui.set_preview_active(bundle.preview_active);
        ui.set_preview_has_image(bundle.preview_has_image);
        ui.set_preview_image(to_slint_image(bundle.preview_image));
        ui.set_preview_text(bundle.preview_text);
        ui.set_preview_info(bundle.preview_info);
        ui.set_menu_visible(bundle.menu_visible);
        ui.set_menu_index(bundle.menu_index);
        ui.set_menu_pinned(bundle.menu_pinned);
        if bundle.menu_position_keyboard && bundle.menu_index >= 0 {
            ui.invoke_menu_position_at(bundle.menu_index);
        }
        let win = ui.window();
        win.set_size(WindowSize::Logical(LogicalSize::new(WIN_W, bundle.height)));
        if bundle.show {
            win_popup::position_near_cursor(win, WIN_W, bundle.height);
            win_popup::apply_style(win);
            let _ = win.show();
            win_popup::store_hwnd(win);
        }
    });
}

fn build_rows(items: &[EntryMeta], thumbs: &HashMap<i64, ImageData>) -> Vec<RowSource> {
    let now = now_ms();
    items
        .iter()
        .enumerate()
        .map(|(i, m)| RowSource {
            id: m.id as i32,
            kind: m.kind.as_i64() as i32,
            icon: kind_icon(m.kind),
            index_label: if i < 9 {
                (i + 1).to_string()
            } else {
                String::new()
            },
            preview: m.preview.clone(),
            sub: kind_sub(m),
            time_ago: time_ago(m.created_ms, now),
            thumb: thumbs.get(&m.id).cloned().unwrap_or_default(),
        })
        .collect()
}

fn kind_icon(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Text => "📝",
        EntryKind::Image => "🖼️",
        EntryKind::Files => "📁",
        EntryKind::RichText => "✨",
    }
}

fn kind_sub(meta: &EntryMeta) -> String {
    if meta.pinned {
        "📌 已置顶".to_string()
    } else {
        String::new()
    }
}

fn filter_label(filter: Option<EntryKind>) -> &'static str {
    match filter {
        None => "全部",
        // 富文本归入「文本」筛选（store 侧文本筛选同时命中两者）
        Some(EntryKind::Text | EntryKind::RichText) => "📝 文本",
        Some(EntryKind::Image) => "🖼️ 图片",
        Some(EntryKind::Files) => "📁 文件",
    }
}

fn cycle_filter(filter: Option<EntryKind>) -> Option<EntryKind> {
    match filter {
        None => Some(EntryKind::Text),
        Some(EntryKind::Text | EntryKind::RichText) => Some(EntryKind::Image),
        Some(EntryKind::Image) => Some(EntryKind::Files),
        Some(EntryKind::Files) => None,
    }
}

fn window_height(rows: usize, search: bool) -> f32 {
    let chrome = HEADER_H + FOOTER_H + if search { SEARCH_H } else { 0.0 };
    let content = (rows as f32 * ROW_H).min(WIN_MAX_H - chrome).max(0.0);
    (chrome + content).clamp(WIN_MIN_H, WIN_MAX_H)
}
