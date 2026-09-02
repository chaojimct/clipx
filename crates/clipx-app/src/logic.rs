//! 逻辑线程：唯一持有弹窗状态（查询/过滤/列表/选中项/可见性），
//! 所有输入源（键盘钩子/鼠标钩子/热键/托盘/处理器）经 AppEvt 汇入此线程，
//! UI 更新统一经 invoke_from_event_loop 回主线程（channel 模式，全平台约定）。

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use clipx_core::{now_ms, time::time_ago, ClipboardGate, EntryKind, EntryMeta};
use clipx_store::Store;
use clipboard_rs::ClipboardContext;
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

#[derive(Debug)]
pub enum AppEvt {
    Key(KeyEvt),
    Toggle,
    Hide,
    ListChanged,
    RowClicked(i32),
    RowDoubleClicked(i32),
    FilterCycle,
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
            #[cfg(windows)]
            foreground_at_show: 0,
        }
    }
}

pub fn spawn(deps: LogicDeps, evt_rx: Receiver<AppEvt>, weak: slint::Weak<PopupWindow>) -> anyhow::Result<()> {
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
                refresh(state, deps, weak, false);
            }
        }
        AppEvt::RowClicked(i) => {
            if state.visible {
                state.selected = (i.max(0) as usize).min(state.items.len().saturating_sub(1));
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
    match k {
        KeyEvt::Esc => hide_popup(state, weak),
        KeyEvt::Enter => do_paste(state, deps, weak, clipboard, state.selected),
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
                handle_key(KeyEvt::Char((b'0' + n) as char), state, deps, weak, clipboard);
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
                refresh(state, deps, weak, false);
            }
        }
        KeyEvt::Up => move_selection(state, weak, -1),
        KeyEvt::Down => move_selection(state, weak, 1),
        KeyEvt::PgUp => move_selection(state, weak, -10),
        KeyEvt::PgDn => move_selection(state, weak, 10),
        KeyEvt::Home => {
            state.selected = 0;
            push_ui(state, weak);
        }
        KeyEvt::End => {
            state.selected = state.items.len().saturating_sub(1);
            push_ui(state, weak);
        }
    }
}

fn move_selection(state: &mut State, weak: &slint::Weak<PopupWindow>, delta: i32) {
    let len = state.items.len();
    if len == 0 {
        return;
    }
    let cur = state.selected as i64;
    let next = (cur + delta as i64).clamp(0, len as i64 - 1);
    state.selected = next as usize;
    push_ui(state, weak);
}

fn show_popup(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    state.query.clear();
    state.filter = None;
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

fn do_paste(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    let Some(meta) = state.items.get(idx) else { return };
    let Some(text) = deps.store.get_text(meta.id) else { return };

    deps.gate.arm();
    if let Some(ctx) = clipboard {
        let _ = paste::write_text(ctx, &text);
    }
    hide_popup(state, weak);
    if deps.settings.paste_simulate {
        std::thread::sleep(Duration::from_millis(80));
        paste::send_ctrl_v();
    }
}

fn check_foreground(state: &mut State, weak: &slint::Weak<PopupWindow>) {
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
        let _ = (state, weak);
    }
}

struct UiBundle {
    rows: Vec<RowData>,
    selected: i32,
    search_active: bool,
    search_text: SharedString,
    search_count: SharedString,
    filter_label: &'static str,
    item_count: SharedString,
    height: f32,
    show: bool,
}

fn ui_bundle(state: &State) -> UiBundle {
    UiBundle {
        rows: build_rows(&state.items),
        selected: state.selected as i32,
        search_active: !state.query.is_empty(),
        search_text: state.query.clone().into(),
        search_count: format!("{} 条结果", state.items.len()).into(),
        filter_label: filter_label(state.filter),
        item_count: format!("{} 条", state.items.len()).into(),
        height: window_height(state.items.len(), !state.query.is_empty()),
        show: false,
    }
}

fn push_ui(state: &State, weak: &slint::Weak<PopupWindow>) {
    invoke_ui(weak, ui_bundle(state));
}

fn invoke_ui(weak: &slint::Weak<PopupWindow>, bundle: UiBundle) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_rows(ModelRc::new(VecModel::from(bundle.rows)));
        ui.set_selected_index(bundle.selected);
        ui.set_search_active(bundle.search_active);
        ui.set_search_text(bundle.search_text);
        ui.set_search_count(bundle.search_count);
        ui.set_type_filter_label(bundle.filter_label.into());
        ui.set_item_count(bundle.item_count);
        let win = ui.window();
        win.set_size(WindowSize::Logical(LogicalSize::new(WIN_W, bundle.height)));
        if bundle.show {
            win_popup::position_near_cursor(&win, WIN_W, bundle.height);
            win_popup::apply_style(&win);
            let _ = win.show();
            win_popup::store_hwnd(&win);
        }
    });
}

fn build_rows(items: &[EntryMeta]) -> Vec<RowData> {
    let now = now_ms();
    items
        .iter()
        .enumerate()
        .map(|(i, m)| RowData {
            id: m.id as i32,
            kind: m.kind.as_i64() as i32,
            icon: kind_icon(m.kind).into(),
            index_label: if i < 9 { (i + 1).to_string().into() } else { SharedString::new() },
            preview: m.preview.clone().into(),
            sub: kind_sub(m),
            time_ago: time_ago(m.created_ms, now).into(),
        })
        .collect()
}

fn kind_icon(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Text => "📝",
        EntryKind::Image => "🖼️",
        EntryKind::Files => "📁",
    }
}

fn kind_sub(meta: &EntryMeta) -> SharedString {
    if meta.pinned { "📌 已置顶".into() } else { SharedString::new() }
}

fn filter_label(filter: Option<EntryKind>) -> &'static str {
    match filter {
        None => "全部",
        Some(EntryKind::Text) => "📝 文本",
        Some(EntryKind::Image) => "🖼️ 图片",
        Some(EntryKind::Files) => "📁 文件",
    }
}

fn cycle_filter(filter: Option<EntryKind>) -> Option<EntryKind> {
    match filter {
        None => Some(EntryKind::Text),
        Some(EntryKind::Text) => Some(EntryKind::Image),
        Some(EntryKind::Image) => Some(EntryKind::Files),
        Some(EntryKind::Files) => None,
    }
}

fn window_height(rows: usize, search: bool) -> f32 {
    let chrome = HEADER_H + FOOTER_H + if search { SEARCH_H } else { 0.0 };
    let content = (rows as f32 * ROW_H).min(WIN_MAX_H - chrome).max(0.0);
    (chrome + content).clamp(WIN_MIN_H, WIN_MAX_H)
}
