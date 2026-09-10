//! 逻辑线程：唯一持有弹窗状态（查询/过滤/列表/选中项/预览/可见性），
//! 所有输入源（键盘钩子/鼠标钩子/热键/托盘/处理器/OCR）经 AppEvt 汇入此线程，
//! UI 更新统一经 invoke_from_event_loop 回主线程（channel 模式，全平台约定）。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use clipboard_rs::ClipboardContext;
use clipx_core::pinyin::to_pinyin_blob;
use clipx_core::{now_ms, time::time_ago, ClipboardGate, EntryKind, EntryMeta, NewEntry};
use clipx_store::Store;
use slint::{ComponentHandle, LogicalSize, Model, ModelRc, SharedString, VecModel, WindowSize};

use crate::keyboard_hook::{self, KeyEvt};
use crate::settings::{QuickPaste, Settings};
use crate::{mouse_hook, paste, win_popup, MenuRow, PopupWindow, RowData};

const PREVIEW_W: f32 = 440.0;
const WIN_MIN_H: f32 = 200.0;
const HEADER_H: f32 = 44.0;
const SEARCH_H: f32 = 40.0;
const FOOTER_H: f32 = 36.0;
/// 圆角卡片外圈留白（阴影），对齐 WPF MainBorder Margin=16。
const POPUP_CHROME: f32 = 16.0;
const ROW_H: f32 = 40.0;
const QUERY_MAX_CHARS: usize = 64;
/// 输入去抖：连打时两次全量刷新最小间隔。首字立即刷保证跟手，
/// 突发输入合并到 200ms pump 超时分支补刷（中文组词/长串粘贴成串进事件时最赚）。
const INPUT_DEBOUNCE_MS: u128 = 90;

/// 对齐 WPF `BitmapImage.DecodePixelWidth = 520`：首屏跟手。
/// 滚轮放大后再解 `PREVIEW_ZOOM_WIDTH`，不 bump preview-seq，缩放位置保持。
const PREVIEW_DECODE_WIDTH: u32 = 520;
/// 放大高清：JPEG 渲染图最多 1280，原图 PNG 解到此宽度。
const PREVIEW_ZOOM_WIDTH: u32 = 1600;
/// 开始请求高清的缩放阈值（略大于 1，避免误触）。
const PREVIEW_HIRES_ZOOM: f32 = 1.15;
/// 预览解码缓存：只存 520 首屏，高清不进缓存（单张 1600 RGBA ~6MB）。
const PREVIEW_CACHE_CAP: usize = 6;
/// 多图文件预览：按路径缓存当前+左右邻居（同样只存 520）。
const FILE_PREVIEW_CACHE_CAP: usize = 4;

/// 跨线程像素载荷：slint::Image 非 Send，逻辑线程只产原始 RGBA，
/// SharedPixelBuffer/Image 在事件循环线程上组装。
#[derive(Clone, Debug)]
pub struct ImageData {
    rgba: std::sync::Arc<[u8]>,
    w: u32,
    h: u32,
}

impl Default for ImageData {
    fn default() -> Self {
        Self {
            rgba: std::sync::Arc::from([]),
            w: 0,
            h: 0,
        }
    }
}

/// 新建设置窗口实例的 weak（`slint::Weak` 未实现 Debug，包一层手写）。
pub struct SettingsWinReady(pub slint::Weak<crate::SettingsWindow>);
impl std::fmt::Debug for SettingsWinReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SettingsWinReady")
    }
}

#[derive(Debug)]
pub enum AppEvt {
    Key(KeyEvt),
    Toggle,
    Hide,
    ListChanged,
    /// OCR 完成（或新图片入队后处理完）：刷新列表与预览中的 OCR 文本
    OcrDone,
    /// 后台解码完成的预览图（gen 过期则丢弃；tier 2=磁盘渲染图先上，3=全解精化）
    PreviewReady {
        gen: u64,
        id: i64,
        tier: u8,
        has_image: bool,
        image: ImageData,
        text: String,
        info: String,
        mono: bool,
        file_images: Vec<String>,
        file_image_idx: usize,
    },
    /// 历史文件条目补完的缩略图
    FileThumbsReady(Vec<(i64, ImageData)>),
    RowClicked(i32),
    RowDoubleClicked(i32),
    FilterCycle,
    /// 批量模式胶囊右键：打开「贴完全部队列」菜单
    BatchMenu,
    /// 右键某行 / Menu 键：打开上下文菜单
    MenuRequest(i32),
    /// 菜单动作执行
    MenuAction(MenuAction),
    /// 点击菜单外关闭
    MenuClose,
    // ===== 快速查找（M4，Explorer 内 Everything 检索）=====
    /// 钩子快速查找事件（QfStart/会话内按键/会话结束）
    QfKey(KeyEvt),
    /// 快速查找浮层列表点击
    QfRowActivated(i32),
    /// 文件夹解析完成（后台 Shell COM 线程回投）
    QfFolderResolved {
        gen: u64,
        folder: Option<String>,
    },
    /// 查询阶段结果回投（gen 过期则丢弃）
    QfResults {
        gen: u64,
        view: crate::explorer_quickfind::QfView,
    },
    // ===== 文件夹跳转（M5d）=====
    /// 全局 Ctrl+G / 托盘：切换 Picker（有框采集跳转，无框全局模式）
    FileJumpToggle,
    /// 对话框前台自动弹出（watcher 线程）
    FileJumpAuto(isize),
    /// Picker 列表点击
    FjRowActivated(i32),
    /// 后台采集完成（gen 过期则丢弃）
    FjCollected {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// Everything 文件夹补充完成（gen 过期则丢弃）
    FjEvResults {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// 后台导航完成
    FjNavigated {
        gen: u64,
        path: String,
        ok: bool,
    },
    /// Picker header 手动拖拽（逻辑像素位移）
    FjDragged(f32, f32),
    // ===== 设置窗口（Phase A）=====
    OpenSettings,
    TrayAutostart,
    /// 托盘：暂停 / 继续采集
    TrayPause,
    /// 托盘：清空历史（两段确认）
    TrayClear,
    SettingBool(String),
    SettingCycle(String),
    SettingRecord(i32),
    /// 设置窗口 Slint 录制覆盖层按键（文本 + MOD_* 修饰位 + 重复标志，不依赖低级钩子）。
    SettingKey(String, u32, bool),
    /// Slint 录制覆盖层松键（裸修饰按住快照维护用）。
    SettingKeyRel(String),
    SettingText(String, String),
    SettingSave,
    SettingCancel,
    SettingClear,
    SettingPage(i32),
    /// 后台进程枚举完成（设置「排除应用」）
    SettingProcs(Vec<String>),
    /// 设置窗口重建完成：回传新实例 weak（替换 deps 中旧 weak）
    SettingsWindowReady(SettingsWinReady),
    ExclAdd,
    ExclDel(i32),
    RuleDel(i32),
    CustomDel(i32),
    CustomImport,
    CustomExport,
    /// 批量模式循环切换（Phase B 实现）
    BatchCycle,
    /// 标题栏图钉：钉住弹窗
    PinWindow,
    /// 标题栏拖拽
    PopupDragged(f32, f32),
    /// 用户拖边缘改尺寸后的逻辑宽高（含 chrome）
    PopupResized { width: f32, height: f32 },
    TextEditChanged(String),
    PhraseEditChanged(String),
    /// 中键预览
    MiddlePreview(i32),
    /// 预览滚轮缩放：阈值以上补高清源图，不 bump preview_seq。
    PreviewZoom(f32),
    /// 滚轮自由滚动跟随：Slint 侧估算的首行（全局行号），越过切片边距时重切片。
    /// 不动选中项（序号/快贴编号随首行重算，与 WPF 一致）。
    ListScrolled(i32),
    /// 托盘：探测文件对话框向导
    TrayProbe,
    /// 托盘：关于 / 检查更新 / 导入导出
    TrayAbout,
    TrayUpdate,
    TrayExport,
    TrayImport,
    /// 后台更新检查结果
    UpdateAvailable(String),
    /// WinEvent：对话框移动/缩放（实时跟随，重算 dock）
    FjDialogMoved,
    /// WinEvent：对话框销毁（Picker 跟着关闭）
    FjDialogGone,
}

/// 右键上下文菜单动作
#[derive(Debug, Clone, Copy)]
pub enum MenuAction {
    Paste,
    Copy,
    Pin,
    Delete,
    /// 加成短语 / 编辑短语
    Phrase,
    Edit,
    OcrPaste,
    PasteAsFile,
    PasteAsJson,
    SaveImage,
    CopyPath,
    FilterSource,
    BatchAll,
}

pub struct LogicDeps {
    pub store: Store,
    pub settings: Settings,
    pub gate: ClipboardGate,
    /// 事件回投通道（快速查找/文件跳转的后台线程经此发回结果）
    pub evt_tx: std::sync::mpsc::Sender<AppEvt>,
    /// 快速查找浮层
    pub qf: slint::Weak<crate::QuickFindWindow>,
    /// 文件夹跳转浮层（M5d）
    pub fj: slint::Weak<crate::FileJumpWindow>,
    /// 设置窗口（Phase A；实例按需重建，weak 经 SettingsWindowReady 回传后更新，
    /// 故套 RefCell——LogicDeps 在逻辑线程独占，无跨线程共享）
    pub settings_win: std::cell::RefCell<slint::Weak<crate::SettingsWindow>>,
    /// 托盘图标（标签/tooltip 刷新；无托盘时为空）
    pub tray: Option<slint::Weak<crate::TrayIcon>>,
    /// 热键热更新通道（设置保存后重注册）
    pub hotkey_tx: std::sync::mpsc::Sender<crate::HotkeySet>,
    /// 设置文件路径（FileJump 收藏/最近写回用）
    pub settings_path: std::path::PathBuf,
    /// 预览渲染图队列（入库/预取）与目录（Tier2 磁盘缓存）
    pub rendition: crate::preview_rendition::RenditionQueue,
    pub rendition_dir: std::path::PathBuf,
}

struct State {
    query: String,
    filter: Option<EntryKind>,
    items: Vec<EntryMeta>,
    selected: usize,
    visible: bool,
    preview_open: bool,
    preview: Option<PreviewData>,
    /// 预览异步代际：方向键连按时丢弃过期解码。
    preview_gen: u64,
    /// 预览条目序号（Slint 重置缩放）。
    preview_seq: u32,
    /// 本条预览已向 worker 要过高清（切条/reload 清掉）。
    preview_hires_requested: bool,
    /// 预览图还在路上：选中已切走，UI 显示加载中。
    preview_loading: bool,
    /// entry_id → 行内缩略图像素（仅解码一次；图片条数受 max_image_items 上限约束）
    thumb_cache: HashMap<i64, ImageData>,
    /// 后台正在解的缩略图，避免滚轮重复 spawn。
    thumb_inflight: HashSet<i64>,
    /// 弹窗隐藏时刻（空闲 trim 的计时锚点）
    hidden_at: Option<std::time::Instant>,
    /// 最近一次显示时刻（失焦关闭宽限期，避免 TOPMOST 刚 show 就误关）
    shown_at: Option<std::time::Instant>,
    /// 右键上下文菜单：是否打开 / 作用于哪一行
    menu_open: bool,
    menu_index: i32,
    /// Alt 在 FIFO/LIFO 或队列非空时开批量菜单（对齐 WPF BatchMenuPopup）
    menu_batch: bool,
    /// 键盘高亮的菜单行
    menu_hl: usize,
    /// 打开菜单时冻结的条目（label, action, danger）
    menu_rows_cache: Vec<(String, String, bool)>,
    /// Menu 键路径：本次 push 需在 Slint 侧按索引计算菜单位置（右键路径不需要）
    menu_keyboard_pending: bool,
    last_click_idx: Option<usize>,
    last_click_at: Option<std::time::Instant>,
    last_paste_at: Option<std::time::Instant>,
    /// FIFO/LIFO 队列（entry id，队首在 [0]）
    batch_queue: Vec<i64>,
    /// 面板主键+Tab：只显示快捷短语
    phrase_only: bool,
    /// 加成/编辑短语浮层
    phrase_edit: Option<PhraseEdit>,
    /// 编辑文本侧栏
    text_edit: Option<TextEdit>,
    /// 弹窗钉住（粘贴/点外不关；与条目置顶分开）
    window_pinned: bool,
    /// 多选锚点（Shift+↑↓ / Shift+单击的固定端，含 selected）
    sel_anchor: usize,
    /// 实际选中下标（列表顺序）。空则视为仅 selected。
    sel_set: BTreeSet<usize>,
    /// 当前列表首个可见行（WPF `_firstVisibleIndex`；序号 1–9 相对此行）
    first_visible: usize,
    /// 推送给 Slint 的切片基址（窗口虚拟化：rows=[row_base,row_base+len)）
    row_base: usize,
    /// 切片右开区间（与 row_base 一起判断滚轮是否仍在当前窗口内）
    row_end: usize,
    /// 上次 Toggle 时刻：按住热键时 WM_HOTKEY 连发，200ms 内去抖，
    /// 否则开关乱闪、终态随机（"再按一次不隐藏"的主因之一）
    last_toggle_at: Option<std::time::Instant>,
    /// Del 二次确认
    pending_delete: Option<i64>,
    /// 预览解码缓存（id → 完整 PreviewData）：来回切图不反复解码，命中零 DB 零解码
    preview_cache: HashMap<i64, PreviewData>,
    /// 多图文件：路径 → 已解码预览（左右切图命中则零 IO）
    file_preview_cache: HashMap<String, ImageData>,
    /// 单预览 worker 投递口（最新优先，替代每导航一起线程）
    preview_loader: PreviewLoader,
    /// 输入去抖：Some(t) 表示 query 已变但列表还没刷（t 为最后一次输入时刻）
    query_dirty_at: Option<std::time::Instant>,
    /// 上次全量刷新的时刻（去抖比较用）
    last_refresh_at: Option<std::time::Instant>,
    /// 来源应用筛选
    source_filter: Option<String>,
    /// 托盘清空两段确认的武装时刻
    clear_armed_at: Option<std::time::Instant>,
    /// 快速查找会话状态（M4）
    qf: crate::explorer_quickfind::QfState,
    /// 文件夹跳转 Picker 状态（M5d）
    fj: crate::filejump::FjState,
    /// 生效中的设置（唯一真相源；设置窗口保存后更新此处）
    settings: Settings,
    /// 状态栏/托盘一次性提示（关于、导出、更新）
    notice: String,
    /// 设置窗口草稿态（Phase A）
    settings_win: crate::settings_win::WinState,
    #[cfg(windows)]
    foreground_at_show: isize,
}

impl State {
    fn new(deps: &LogicDeps, preview_tx: std::sync::mpsc::Sender<PreviewReq>) -> Self {
        Self {
            query: String::new(),
            filter: None,
            items: Vec::new(),
            selected: 0,
            visible: false,
            preview_open: false,
            preview: None,
            preview_gen: 0,
            preview_seq: 0,
            preview_hires_requested: false,
            preview_loading: false,
            thumb_cache: HashMap::new(),
            thumb_inflight: HashSet::new(),
            // 启动即进入空闲计时：弹窗从未弹出的会话（迁移 OCR 回填突发后）
            // 也要周期性 trim，避免分配器滞留的工作集虚高
            hidden_at: Some(std::time::Instant::now()),
            shown_at: None,
            menu_open: false,
            menu_index: -1,
            menu_batch: false,
            menu_hl: 0,
            menu_rows_cache: Vec::new(),
            menu_keyboard_pending: false,
            last_click_idx: None,
            last_click_at: None,
            last_paste_at: None,
            batch_queue: Vec::new(),
            phrase_only: false,
            phrase_edit: None,
            text_edit: None,
            window_pinned: false,
            sel_anchor: 0,
            sel_set: BTreeSet::new(),
            first_visible: 0,
            row_base: 0,
            row_end: 0,
            last_toggle_at: None,
            pending_delete: None,
            preview_cache: HashMap::new(),
            file_preview_cache: HashMap::new(),
            preview_loader: PreviewLoader { tx: preview_tx },
            query_dirty_at: None,
            last_refresh_at: None,
            source_filter: None,
            notice: String::new(),
            clear_armed_at: None,
            qf: crate::explorer_quickfind::QfState::default(),
            fj: crate::filejump::FjState::default(),
            settings: deps.settings.clone(),
            settings_win: crate::settings_win::WinState::default(),
            #[cfg(windows)]
            foreground_at_show: 0,
        }
    }
}

#[derive(Clone)]
struct PreviewData {
    has_image: bool,
    image: ImageData,
    text: String,
    info: String,
    /// JSON 等结构化文本用等宽字体。
    mono: bool,
    file_images: Vec<String>,
    file_image_idx: usize,
}

struct PhraseEdit {
    content: String,
    buffer: String,
}

struct TextEdit {
    id: i64,
    buffer: String,
}

pub fn spawn(
    deps: LogicDeps,
    evt_rx: Receiver<AppEvt>,
    weak: slint::Weak<PopupWindow>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("clipx-logic".into())
        .spawn(move || {
            // 单预览 worker：导航再快也只有一个解码在跑，新的顶掉旧的。
            // 无界 channel 只装小请求（meta+设置快照），收到后吸干只留最新。
            let (preview_tx, preview_rx) = std::sync::mpsc::channel::<PreviewReq>();
            {
                let rx = preview_rx;
                let store = deps.store.clone();
                let evt_tx = deps.evt_tx.clone();
                let rdir = deps.rendition_dir.clone();
                let rendition = deps.rendition.clone();
                let _ = std::thread::Builder::new()
                    .name("clipx-preview".into())
                    .spawn(move || preview_worker(rx, store, evt_tx, rdir, rendition));
            }
            let mut state = State::new(&deps, preview_tx);
            let clipboard = ClipboardContext::new().ok();
            {
                let tx = deps.evt_tx.clone();
                crate::win_popup::set_resize_handler(move |w, h| {
                    let _ = tx.send(AppEvt::PopupResized {
                        width: w,
                        height: h,
                    });
                });
            }
            loop {
                match evt_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(evt) => handle(evt, &mut state, &deps, &weak, clipboard.as_ref()),
                    Err(RecvTimeoutError::Timeout) => {
                        flush_query_dirty_aged(&mut state, &deps, &weak);
                        check_foreground(&mut state, &deps, &weak);
                    }
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
            // 按住连发去抖（WM_HOTKEY 在按住期间按 typematic 速率重发）。
            let now = std::time::Instant::now();
            if state
                .last_toggle_at
                .map(|t| now.duration_since(t) < Duration::from_millis(200))
                .unwrap_or(false)
            {
                return;
            }
            state.last_toggle_at = Some(now);
            if state.fj.visible {
                fj_hide(state, deps);
            }
            if state.visible {
                hide_popup(state, weak);
            } else {
                // 主弹窗弹出时终结快速查找会话（钩子键路由切回弹窗优先）
                crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
                show_popup(state, deps, weak);
            }
        }
        AppEvt::Hide => {
            if state.visible && state.settings.hide_on_click_outside && !state.window_pinned {
                hide_popup(state, weak);
            }
            if state.fj.visible {
                // 停靠模式粘性：点对话框/外部不关闭（对齐 WPF sticky：
                // “贴靠、不因点文件窗/失焦而关”），否则拖框起手的第一下
                // 鼠标按下就把窗关了，跟随永无机会跑。
                // 关闭途径：Esc / Ctrl+G / 托盘 / 对话框销毁。
                // 全局模式（无对话框）点外关闭。
                if state.fj.global_mode || state.fj.dialog == 0 {
                    fj_hide(state, deps);
                }
            }
        }
        AppEvt::ListChanged => {
            if state.settings.batch_mode != "Off" {
                batch_enqueue_latest(state, deps, clipboard);
            }
            if state.visible {
                ensure_window_thumbs(state, deps);
                refresh(state, deps, weak, false);
            }
        }
        AppEvt::OcrDone => {
            // OCR 文本 baked 进预览缓存：整清，下次 reload 重解（低频事件，开销可忽略）。
            state.preview_cache.clear();
            if state.visible {
                reload_preview_if_open(state, deps);
                refresh(state, deps, weak, false);
            }
        }
        AppEvt::PreviewReady {
            gen,
            id,
            tier,
            has_image,
            image,
            text,
            info,
            mono,
            file_images,
            file_image_idx,
        } => {
            // 先按路径收下（含邻居预取 / 过期 gen），切图才跟手。高清不进路径缓存。
            if !file_images.is_empty() && tier < 4 {
                if let Some(path) = file_images.get(file_image_idx) {
                    remember_file_preview(state, path.clone(), image.clone());
                }
            }
            if !state.visible || !state.preview_open || gen != state.preview_gen {
                return;
            }
            if state.items.get(state.selected).map(|m| m.id) != Some(id) {
                return;
            }
            // 邻居预取：只进缓存，不把正在看的图换掉。
            if !file_images.is_empty() {
                let cur = state.preview.as_ref().map(|p| p.file_image_idx).unwrap_or(0);
                if cur != file_image_idx {
                    return;
                }
            }
            let preview = PreviewData {
                has_image,
                image,
                text,
                info,
                mono,
                file_images,
                file_image_idx,
            };
            // 只缓存 520 首屏；高清（tier 4）不进缓存，避免常驻内存超标。
            if (2..4).contains(&tier)
                && state
                    .items
                    .iter()
                    .find(|m| m.id == id)
                    .is_some_and(|m| m.kind == EntryKind::Image)
            {
                if state.preview_cache.len() >= PREVIEW_CACHE_CAP {
                    state.preview_cache.clear();
                }
                state.preview_cache.insert(id, preview.clone());
                // 邻居预取：上下各一张图片进渲染队列，切过去时 Tier2 秒出。
                if let Some(pos) = state.items.iter().position(|m| m.id == id) {
                    for delta in [-1i64, 1] {
                        let npos = pos as i64 + delta;
                        if npos < 0 {
                            continue;
                        }
                        if let Some(nm) = state.items.get(npos as usize) {
                            if nm.kind == EntryKind::Image
                                && !state.preview_cache.contains_key(&nm.id)
                            {
                                deps.rendition.request(nm.id);
                            }
                        }
                    }
                }
            }
            state.preview = Some(preview);
            state.preview_loading = false;
            // 只换图，不再 set selected-index（避免和大图上传挤在同一帧）。
            push_preview_image(state, weak, !state.preview_hires_requested && tier < 4);
        }
        AppEvt::FileThumbsReady(thumbs) => {
            let mut patches = Vec::new();
            for (id, img) in thumbs {
                state.thumb_inflight.remove(&id);
                if img.w == 0 {
                    continue;
                }
                if let Some(idx) = state.items.iter().position(|m| m.id == id) {
                    patches.push((idx, id, img.clone()));
                }
                state.thumb_cache.entry(id).or_insert(img);
            }
            prune_thumb_cache(state);
            if state.visible && !patches.is_empty() {
                patch_row_thumbs(weak, patches);
            }
        }
        AppEvt::RowClicked(i) => {
            if state.visible {
                // i 为切片相对号，还原全局行号。
                let idx = (state.row_base + i.max(0) as usize).min(state.items.len().saturating_sub(1));
                let shift = keyboard_hook::click_shift();
                let ctrl = keyboard_hook::click_ctrl();
                let is_double = !shift
                    && !ctrl
                    && state.last_click_idx == Some(idx)
                    && state
                        .last_click_at
                        .map(|t| t.elapsed() < Duration::from_millis(400))
                        .unwrap_or(false);
                if shift {
                    select_extend(state, idx);
                } else if ctrl {
                    select_toggle(state, idx);
                } else {
                    select_single(state, idx);
                }
                state.last_click_idx = Some(idx);
                state.last_click_at = Some(std::time::Instant::now());
                if shift || ctrl {
                    reload_preview_if_open(state, deps);
                    push_ui(state, weak);
                } else if !state.settings.paste_double_click || is_double {
                    activate_item(state, deps, weak, clipboard, idx);
                } else {
                    reload_preview_if_open(state, deps);
                    push_ui(state, weak);
                }
            }
        }
        AppEvt::RowDoubleClicked(i) => {
            if state.visible {
                let idx = (state.row_base + i.max(0) as usize)
                    .min(state.items.len().saturating_sub(1));
                activate_item(state, deps, weak, clipboard, idx);
            }
        }
        AppEvt::FilterCycle => {
            if state.visible {
                state.phrase_only = false;
                if state.source_filter.take().is_some() {
                    refresh(state, deps, weak, true);
                } else {
                    state.filter = cycle_filter(state.filter);
                    refresh(state, deps, weak, true);
                }
            }
        }
        AppEvt::MenuRequest(i) => {
            if state.visible {
                let idx =
                    (state.row_base + i.max(0) as usize).min(state.items.len().saturating_sub(1));
                select_single(state, idx);
                open_menu(state, deps, idx, false, false);
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
            if matches!(k, KeyEvt::WinV) {
                if state.fj.visible {
                    fj_hide(state, deps);
                }
                if state.visible {
                    hide_popup(state, weak);
                } else {
                    crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
                    show_popup(state, deps, weak);
                }
                return;
            }
            if matches!(k, KeyEvt::BatchAdvance) {
                batch_advance(state, deps, weak, clipboard);
                return;
            }
            // 面板可见时呼出热键由钩子翻成 KeyEvt，勿再当搜索字符。
            if matches!(k, KeyEvt::Toggle) {
                handle(AppEvt::Toggle, state, deps, weak, clipboard);
                return;
            }
            if matches!(k, KeyEvt::FileJumpHotkey) {
                handle(AppEvt::FileJumpToggle, state, deps, weak, clipboard);
                return;
            }
            if matches!(k, KeyEvt::BatchHotkey) {
                handle(AppEvt::BatchCycle, state, deps, weak, clipboard);
                return;
            }
            // 设置窗口录制优先（主弹窗此时必隐藏）。
            if let KeyEvt::RecordVk(vk, mods) = k {
                if state.settings_win.open {
                    crate::settings_win::handle_record_vk(&mut state.settings_win, vk, mods);
                    crate::settings_win::patch_recording(&deps.settings_win.borrow(), &state.settings_win);
                }
                return;
            }
            // FileJump 可见时键路由归 Picker（主弹窗此时必隐藏，互斥）
            if state.fj.visible {
                fj_key(k, state, deps);
                return;
            }
            if state.visible {
                handle_key(k, state, deps, weak, clipboard);
            }
        }
        // ===== 设置窗口（Phase A）=====
        AppEvt::OpenSettings => {
            if state.visible {
                hide_popup(state, weak);
            }
            if state.fj.visible {
                fj_hide(state, deps);
            }
            crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
            crate::settings_win::open(
                &mut state.settings_win,
                &state.settings,
                &deps.settings_path,
                deps.evt_tx.clone(),
            );
            crate::settings_win::request_procs_if_needed(
                &mut state.settings_win,
                2,
                &deps.evt_tx,
            );
        }
        AppEvt::TrayAutostart => {
            state.settings.run_at_startup = !state.settings.run_at_startup;
            let ok = crate::autostart::set(
                state.settings.run_at_startup,
                state.settings.run_as_admin,
            );
            if !ok {
                state.settings.run_at_startup = !state.settings.run_at_startup;
            }
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
            refresh_tray(state, deps);
        }
        AppEvt::TrayPause => {
            let paused = crate::policy::toggle_capture_paused();
            if paused && state.visible {
                // 暂停不影响已打开的列表，只挡后续入库
            }
            refresh_tray(state, deps);
        }
        AppEvt::TrayClear => {
            tray_clear_flow(state, deps, weak);
        }
        AppEvt::SettingBool(name) => {
            if state.settings_win.open {
                crate::settings_win::handle_bool(&mut state.settings_win, &name);
                // 开关已在 Slint 侧翻转；仅「自动弹出」会影响跟随行显隐。
                if name == "autopopup" {
                    crate::settings_win::patch_autopopup(
                        &deps.settings_win.borrow(),
                        &state.settings_win,
                    );
                }
            }
        }
        AppEvt::SettingCycle(name) => {
            if state.settings_win.open {
                crate::settings_win::handle_cycle(&mut state.settings_win, &name, weak);
                crate::settings_win::patch_cycle(&deps.settings_win.borrow(), &state.settings_win, &name);
            }
        }
        AppEvt::SettingRecord(slot) => {
            if state.settings_win.open {
                crate::settings_win::handle_record(&mut state.settings_win, slot);
                crate::settings_win::patch_recording(&deps.settings_win.borrow(), &state.settings_win);
            }
        }
        AppEvt::SettingKey(text, mods, repeat) => {
            if state.settings_win.open {
                crate::settings_win::handle_slint_key(
                    &mut state.settings_win,
                    &text,
                    mods,
                    repeat,
                );
                crate::settings_win::patch_recording(&deps.settings_win.borrow(), &state.settings_win);
            }
        }
        AppEvt::SettingKeyRel(text) => {
            if state.settings_win.open {
                crate::settings_win::handle_slint_rel(&mut state.settings_win, &text);
            }
        }
        AppEvt::SettingPage(page) => {
            if state.settings_win.open {
                crate::settings_win::request_procs_if_needed(
                    &mut state.settings_win,
                    page,
                    &deps.evt_tx,
                );
            }
        }
        AppEvt::SettingProcs(names) => {
            if state.settings_win.open {
                crate::settings_win::apply_procs(&mut state.settings_win, names);
                crate::settings_win::push_proc_list(&deps.settings_win.borrow(), &state.settings_win);
            }
        }
        AppEvt::SettingsWindowReady(w) => {
            *deps.settings_win.borrow_mut() = w.0;
        }
        AppEvt::SettingText(field, text) => {
            if state.settings_win.open {
                crate::settings_win::handle_text(&mut state.settings_win, &field, text);
                // 文本框/滑条不整窗重推；短语列表变更才刷新模型。
                if field == "phrase-add"
                    || field == "phrase-del"
                    || field == "psel"
                    || field == "ptrig"
                    || field == "pbody"
                {
                    crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
                }
            }
        }
        AppEvt::SettingSave => {
            if state.settings_win.open
                && crate::settings_win::handle_save(&mut state.settings_win)
                && apply_settings(state, deps, weak)
            {
                state.settings_win.open = false;
                crate::settings_win::hide(&deps.settings_win.borrow());
            }
            crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
        }
        AppEvt::SettingCancel => {
            if state.settings_win.open {
                // 主题预览回滚（WPF 语义），其余 pending 直接丢弃。
                crate::settings_win::apply_theme(&state.settings.theme, weak);
                state.settings_win.open = false;
                crate::settings_win::hide(&deps.settings_win.borrow());
            }
        }
        AppEvt::SettingClear => {
            if state.settings_win.open {
                settings_clear_flow(state, deps);
            }
        }
        AppEvt::ExclAdd => {
            if state.settings_win.open {
                settings_excl_add(state);
                crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
            }
        }
        AppEvt::ExclDel(i) => {
            if state.settings_win.open {
                settings_excl_del(state, i);
                crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
            }
        }
        AppEvt::RuleDel(i) => {
            if state.settings_win.open {
                let idx = i.max(0) as usize;
                if idx < state.settings_win.draft_passthrough_len() {
                    state.settings_win.draft_rule_remove(idx);
                    crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
                }
            }
        }
        AppEvt::CustomDel(i) => {
            if state.settings_win.open {
                settings_custom_del(state, i);
                crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
            }
        }
        AppEvt::CustomImport => {
            if state.settings_win.open {
                settings_custom_import(state);
                crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
            }
        }
        AppEvt::CustomExport => {
            if state.settings_win.open {
                settings_custom_export(state);
                crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
            }
        }
        AppEvt::BatchCycle => {
            cycle_batch_mode(state, deps, weak);
        }
        AppEvt::BatchMenu => {
            if state.visible {
                let idx = if state.items.is_empty() {
                    0
                } else {
                    state.selected.min(state.items.len() - 1)
                };
                open_menu(state, deps, idx, false, true);
                push_ui(state, weak);
            }
        }
        AppEvt::PinWindow => {
            state.window_pinned = !state.window_pinned;
            if state.visible {
                push_ui(state, weak);
            }
        }
        AppEvt::PopupDragged(dx, dy) => {
            let weak_ui = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_ui.upgrade() {
                    crate::win_popup::drag_picker_by(&ui.window(), dx, dy);
                }
            });
        }
        AppEvt::PopupResized { width, height } => {
            let chrome = POPUP_CHROME * 2.0;
            let extra = if state.preview_open { PREVIEW_W } else { 0.0 };
            state.settings.popup_width =
                (width - chrome - extra).clamp(280.0, 1200.0) as f64;
            let content_h = (height - chrome).clamp(200.0, 900.0);
            state.settings.popup_height = content_h as f64;
            state.settings.popup_max_height =
                state.settings.popup_max_height.max(content_h as f64);
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
        }
        AppEvt::TextEditChanged(s) => {
            if let Some(e) = state.text_edit.as_mut() {
                e.buffer = s;
            }
        }
        AppEvt::PhraseEditChanged(s) => {
            if let Some(e) = state.phrase_edit.as_mut() {
                e.buffer = s;
            }
        }
        AppEvt::PreviewZoom(z) => request_preview_hires(state, z),
        AppEvt::MiddlePreview(i) => {
            if i >= 0 {
                let idx = (state.row_base + i as usize).min(state.items.len().saturating_sub(1));
                select_single(state, idx);
                if !state.preview_open {
                    toggle_preview(state, deps, weak);
                } else {
                    reload_preview_if_open(state, deps);
                    push_ui(state, weak);
                }
            }
        }
        AppEvt::ListScrolled(approx) => {
            if !state.visible || state.items.is_empty() {
                return;
            }
            let approx = (approx.max(0) as usize).min(state.items.len().saturating_sub(1));
            if approx != state.first_visible {
                state.first_visible = approx;
                clamp_first_visible(state);
            }
            request_window_thumbs(state, deps);
        }
        AppEvt::TrayProbe => tray_probe_dialog(state, deps),
        AppEvt::TrayAbout => {
            notify(
                state,
                deps,
                format!("clipx {} · 对齐并超越 WPF 1.9.8", env!("CARGO_PKG_VERSION")),
            );
        }
        AppEvt::TrayUpdate => check_updates_now(state, deps),
        AppEvt::TrayExport => export_history(state, deps),
        AppEvt::TrayImport => import_history(state, deps, weak),
        // ===== 快速查找（M4）=====
        AppEvt::QfKey(k) => {
            // 主弹窗可见期间钩子不会产生 Qf 事件，此处防御性忽略
            if !state.visible {
                let max = state.settings.explorer_everything_quickfind_max_results;
                crate::explorer_quickfind::handle_key(
                    k,
                    &mut state.qf,
                    max,
                    &deps.evt_tx,
                    &deps.qf,
                    &state.settings.explorer_quickfind_open_mode,
                );
            }
        }
        AppEvt::QfRowActivated(i) => {
            crate::explorer_quickfind::activate_index(
                i.max(0) as usize,
                &mut state.qf,
                &deps.qf,
                &state.settings.explorer_quickfind_open_mode,
            );
        }
        AppEvt::QfFolderResolved { gen, folder } => {
            let max = state.settings.explorer_everything_quickfind_max_results;
            crate::explorer_quickfind::apply_folder(
                gen,
                folder,
                &mut state.qf,
                max,
                &deps.evt_tx,
                &deps.qf,
            );
        }
        AppEvt::QfResults { gen, view } => {
            crate::explorer_quickfind::apply_results(gen, view, &mut state.qf, &deps.qf);
        }
        // ===== 文件夹跳转（M5d）=====
        AppEvt::FileJumpToggle => {
            if !state.settings.filejump_enabled {
                return;
            }
            if state.fj.visible {
                let within = state
                    .fj
                    .last_hotkey_at
                    .map(|t| t.elapsed() < Duration::from_millis(state.settings.filejump_show_delay_ms.max(400)))
                    .unwrap_or(false);
                if within && !state.fj.filtered.is_empty() {
                    fj_activate(state, deps);
                } else {
                    fj_hide(state, deps);
                }
                return;
            }
            if state.visible {
                hide_popup(state, weak);
            }
            crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
            #[cfg(windows)]
            let found = clipx_filejump::dialog::win::resolve_from_foreground();
            #[cfg(not(windows))]
            let found: Option<isize> = None;
            fj_show(state, deps, found, false);
            state.fj.last_hotkey_at = Some(std::time::Instant::now());
        }
        AppEvt::FileJumpAuto(d) => {
            if !state.settings.filejump_enabled || state.fj.visible || state.visible {
                return;
            }
            crate::explorer_quickfind::end_if_active(&mut state.qf, &deps.qf);
            fj_show(state, deps, Some(d), state.settings.filejump_auto_jump);
        }
        AppEvt::FjRowActivated(i) => {
            if state.fj.visible {
                let idx = i.max(0) as usize;
                if idx < state.fj.filtered.len() {
                    state.fj.selected = idx;
                    fj_activate(state, deps);
                }
            }
        }
        AppEvt::FjCollected { gen, items } => {
            if !state.fj.visible || state.fj.gen != gen {
                return;
            }
            state.fj.current_folder = items
                .iter()
                .find(|c| c.source == clipx_filejump::CandidateSource::Current)
                .map(|c| c.path.clone());
            state.fj.items = items;
            fj_apply_filter(state);
            state.fj.selected = 0;
            state.fj.hint.clear();
            // 自动跳转：首个候选直接跳（WPF FileJumpAutoOnFirstClick）。
            if state.fj.pending_auto_jump {
                state.fj.pending_auto_jump = false;
                if !state.fj.filtered.is_empty() && !state.fj.global_mode {
                    fj_activate(state, deps);
                    return;
                }
            }
            crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
        }
        AppEvt::FjEvResults { gen, items } => {
            if !state.fj.visible || state.fj.gen != gen {
                return;
            }
            let sel_path = state
                .fj
                .filtered
                .get(state.fj.selected)
                .and_then(|&i| state.fj.items.get(i))
                .map(|c| c.path.clone());
            let keep = state.fj.current_folder.as_deref();
            state.fj.items.retain(|c| {
                if c.source == clipx_filejump::CandidateSource::Everything {
                    return false;
                }
                if c.source != clipx_filejump::CandidateSource::Current {
                    return true;
                }
                keep.map(|k| k.eq_ignore_ascii_case(&c.path)).unwrap_or(false)
            });
            let mut seen: std::collections::HashSet<String> = state
                .fj
                .items
                .iter()
                .map(|c| c.path.to_lowercase())
                .collect();
            let mut children = Vec::new();
            let mut ev = Vec::new();
            for c in items {
                if !seen.insert(c.path.to_lowercase()) {
                    continue;
                }
                match c.source {
                    clipx_filejump::CandidateSource::Current => children.push(c),
                    clipx_filejump::CandidateSource::Everything => ev.push(c),
                    _ => {}
                }
            }
            let insert_at = state
                .fj
                .items
                .iter()
                .position(|c| {
                    c.source == clipx_filejump::CandidateSource::Current
                        && keep.map(|k| k.eq_ignore_ascii_case(&c.path)).unwrap_or(false)
                })
                .map(|i| i + 1)
                .unwrap_or(0);
            state.fj.items.splice(insert_at..insert_at, children);
            state.fj.items.extend(ev);
            fj_apply_filter(state);
            // 选中跟随原条目。
            if let Some(p) = sel_path {
                if let Some(pos) = state
                    .fj
                    .filtered
                    .iter()
                    .position(|&i| state.fj.items.get(i).map(|c| c.path == p).unwrap_or(false))
                {
                    state.fj.selected = pos;
                } else {
                    state.fj.selected = 0;
                }
            }
            crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
        }
        AppEvt::FjDragged(dx, dy) => {
            if state.fj.visible {
                let fj = deps.fj.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = fj.upgrade() {
                        crate::win_popup::drag_picker_by(&ui.window(), dx, dy);
                    }
                });
            }
        }
        AppEvt::FjDialogMoved => {
            // WinEvent 实时跟随：重算 dock（位置不变内部跳过）。
            // 对齐 WPF：手动拖拽的偏移在下一次 owner 移动时被 dock 重算覆盖。
            if state.fj.visible && state.fj.dialog != 0 {
                state.fj.last_dlg_rect = crate::win_popup::dialog_rect(state.fj.dialog);
                if !crate::win_popup::redock_picker(state.fj.dialog) {
                    fj_hide(state, deps);
                }
            }
        }
        AppEvt::FjDialogGone => {
            if state.fj.visible {
                fj_hide(state, deps);
            }
        }
        AppEvt::FjNavigated { gen, path, ok } => {
            if !state.fj.visible || state.fj.gen != gen {
                return;
            }
            state.fj.navigating = false;
            if ok {
                // 跳转成功保留弹窗（对齐 WPF“单击条目只导航不关闭”），可继续跳。
                let mut recent: Vec<String> = state.settings.filejump_recent.clone();
                crate::filejump::push_recent(
                    &deps.settings_path,
                    &mut recent,
                    &path,
                    state.settings.filejump_recent_max,
                );
                let n = state
                    .settings
                    .filejump_confirm_counts
                    .entry(path.clone())
                    .or_insert(0);
                *n += 1;
                let cnt = *n;
                if cnt >= state.settings.filejump_auto_add_min {
                    let already = state
                        .settings
                        .filejump_favorites
                        .iter()
                        .any(|f| f.path().eq_ignore_ascii_case(&path));
                    if !already {
                        state
                            .settings
                            .filejump_favorites
                            .push(crate::settings::FolderFavorite::full(String::new(), path.clone()));
                    }
                }
                let _ = crate::settings::save(&deps.settings_path, &state.settings);
                state.fj.hint = format!("已跳转：{path}");
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            } else {
                state.fj.hint = "跳转失败（注入与键盘回退均未命中），换一条试试".to_string();
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            }
        }
        AppEvt::UpdateAvailable(tag) => {
            if state.settings.last_update_tag.as_deref() == Some(tag.as_str()) {
                return;
            }
            state.settings.last_update_tag = Some(tag.clone());
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
            notify(state, deps, format!("发现新版本 {tag}"));
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
    if state.text_edit.is_some() {
        handle_text_edit_key(k, state, deps, weak);
        return;
    }
    if state.phrase_edit.is_some() {
        handle_phrase_edit_key(k, state, deps, weak);
        return;
    }
    // 菜单打开期间：↑↓ 高亮、Enter 执行、Esc/Alt 关闭（对齐 WPF 右键/批量菜单键盘）。
    if state.menu_open {
        match k {
            KeyEvt::Esc | KeyEvt::AltTap => {
                state.menu_open = false;
                push_ui(state, weak);
            }
            KeyEvt::Up => {
                let n = state.menu_rows_cache.len();
                if n > 0 {
                    state.menu_hl = (state.menu_hl + n - 1) % n;
                }
                push_ui(state, weak);
            }
            KeyEvt::Down => {
                let n = state.menu_rows_cache.len();
                if n > 0 {
                    state.menu_hl = (state.menu_hl + 1) % n;
                }
                push_ui(state, weak);
            }
            KeyEvt::Enter => {
                if let Some(action) = menu_action_at(state, state.menu_hl) {
                    state.menu_open = false;
                    menu_action(action, state, deps, weak, clipboard);
                } else {
                    state.menu_open = false;
                    push_ui(state, weak);
                }
            }
            _ => {}
        }
        return;
    }
    // 非输入键先把挂起的查询刷了，保证导航/粘贴看到最新列表。
    // 输入键（Char/Backspace/Digit 经 Char）走去抖，不在这里刷。
    if !matches!(k, KeyEvt::Char(_) | KeyEvt::Backspace | KeyEvt::Digit(_)) {
        flush_query_dirty(state, deps, weak);
    }
    match k {
        KeyEvt::Esc => {
            if state.preview_open {
                state.preview_open = false;
                state.preview = None;
                push_ui(state, weak);
            } else if state.pending_delete.is_some() {
                state.pending_delete = None;
                push_ui(state, weak);
            } else if !state.query.is_empty() {
                state.query.clear();
                refresh(state, deps, weak, true);
            } else {
                hide_popup(state, weak);
            }
        }
        KeyEvt::Enter => paste_selection(state, deps, weak, clipboard, false),
        KeyEvt::CtrlEnter => paste_selection(state, deps, weak, clipboard, true),
        KeyEvt::ShiftEnter => paste_ocr(state, deps, weak, clipboard, state.selected),
        KeyEvt::AltTap => {
            if !state.items.is_empty() {
                open_menu(
                    state,
                    deps,
                    state.selected,
                    true,
                    prefer_batch_menu(state),
                );
                push_ui(state, weak);
            }
        }
        KeyEvt::BatchAdvance => batch_advance(state, deps, weak, clipboard),
        KeyEvt::Space => toggle_preview(state, deps, weak),
        KeyEvt::PinToggle => {
            if let Some(meta) = state.items.get(state.selected).cloned() {
                if is_phrase_id(meta.id) {
                    return;
                }
                let _ = deps.store.toggle_pin(meta.id);
                // 置顶条目浮动到顶部：选中跟随原条目而非原索引
                refresh(state, deps, weak, false);
                select_single(
                    state,
                    state
                        .items
                        .iter()
                        .position(|m| m.id == meta.id)
                        .unwrap_or(state.selected),
                );
                ensure_selection_visible(state);
                reload_preview_if_open(state, deps);
                push_ui(state, weak);
            }
        }
        KeyEvt::Menu => {
            if !state.items.is_empty() {
                open_menu(state, deps, state.selected, true, false);
                push_ui(state, weak);
            }
        }
        KeyEvt::Char(c) => {
            if state.query.chars().count() < QUERY_MAX_CHARS {
                state.query.push(c);
                refresh_input(state, deps, weak);
            }
        }
        KeyEvt::Digit(n) => {
            // WPF：裸数字进搜索；快贴只走主键+1..9（QuickNum）
            handle_key(
                KeyEvt::Char((b'0' + n) as char),
                state,
                deps,
                weak,
                clipboard,
            );
        }
        // 面板主键+数字：按当前可见窗口的显示序号快贴（WPF PasteByIndex）
        KeyEvt::QuickNum(n) => {
            if let Some(idx) = index_of_display(n, state.first_visible, state.items.len()) {
                activate_item(state, deps, weak, clipboard, idx);
            }
        }
        KeyEvt::QuickTab => {
            state.phrase_only = !state.phrase_only;
            if state.phrase_only {
                state.filter = None;
            }
            refresh(state, deps, weak, true);
        }
        KeyEvt::Tab => {
            state.phrase_only = false;
            if state.source_filter.take().is_some() {
                refresh(state, deps, weak, true);
            } else {
                state.filter = cycle_filter(state.filter);
                refresh(state, deps, weak, true);
            }
        }
        KeyEvt::Backspace => {
            if state.query.pop().is_some() {
                refresh_input(state, deps, weak);
            }
        }
        KeyEvt::Delete => {
            if let Some(meta) = state.items.get(state.selected).cloned() {
                if state.pending_delete == Some(meta.id) {
                    delete_item(state, deps, meta.id);
                    state.pending_delete = None;
                    refresh(state, deps, weak, false);
                    reload_preview_if_open(state, deps);
                } else {
                    state.pending_delete = Some(meta.id);
                    push_ui(state, weak);
                }
            }
        }
        KeyEvt::Up => move_selection(state, deps, weak, -1, false),
        KeyEvt::Down => move_selection(state, deps, weak, 1, false),
        KeyEvt::ShiftUp => move_selection(state, deps, weak, -1, true),
        KeyEvt::ShiftDown => move_selection(state, deps, weak, 1, true),
        KeyEvt::PgUp | KeyEvt::Left => {
            if !preview_step_image(state, deps, weak, -1) {
                scroll_page(state, deps, weak, -1);
            }
        }
        KeyEvt::PgDn | KeyEvt::Right => {
            if !preview_step_image(state, deps, weak, 1) {
                scroll_page(state, deps, weak, 1);
            }
        }
        KeyEvt::Home => {
            select_single(state, 0);
            state.first_visible = 0;
            state.pending_delete = None;
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
        KeyEvt::End => {
            select_single(state, state.items.len().saturating_sub(1));
            ensure_selection_visible(state);
            state.pending_delete = None;
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
        // Qf 事件经 AppEvt::QfKey 独立路由（见 main.rs 转发器），不会进入此分支
        _ => {}
    }
}

fn move_selection(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    delta: i32,
    expand: bool,
) {
    let len = state.items.len();
    if len == 0 {
        return;
    }
    let old_first = state.first_visible;
    let was_multi = state.sel_set.len() > 1;
    let cur = state.selected as i64;
    let next = (cur + delta as i64).clamp(0, len as i64 - 1) as usize;
    if expand {
        select_extend(state, next);
    } else {
        select_single(state, next);
    }
    ensure_selection_visible(state);
    state.pending_delete = None;
    if state.preview_open {
        if let Some(meta) = state.items.get(state.selected).cloned() {
            begin_preview_nav(state, &meta);
            push_selection_only(state, weak);
            dispatch_preview_load(state, deps, &meta);
            if meta.kind != EntryKind::Image && meta.kind != EntryKind::Files {
                push_preview_image(state, weak, true);
            }
        }
    }
    // 同一页内单选：选中已在上面推过，不再重复。
    // 多选 / 翻页必须重建 rows.picked。
    if !expand && !was_multi && state.first_visible == old_first {
        if !state.preview_open {
            push_preview_ui(state, weak);
        }
    } else {
        push_ui(state, weak);
    }
}

/// Space 切换预览（WPF 版行为）：打开时加载当前选中项，
/// 切换选中项后预览内容跟随。
fn toggle_preview(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    if state.preview_open {
        state.preview_open = false;
        state.preview = None;
        state.preview_loading = false;
    } else {
        if state.items.is_empty() {
            return;
        }
        state.preview = Some(preview_placeholder(
            &state.items[state.selected],
            &state.thumb_cache,
        ));
        state.preview_open = true;
        reload_preview_if_open(state, deps);
    }
    push_ui(state, weak);
}

fn reload_preview_if_open(state: &mut State, deps: &LogicDeps) {
    if !state.preview_open {
        return;
    }
    let Some(meta) = state.items.get(state.selected).cloned() else {
        state.preview = None;
        state.preview_loading = false;
        return;
    };
    begin_preview_nav(state, &meta);
    dispatch_preview_load(state, deps, &meta);
}

/// 切条：bump 代际、进入加载态（不阻塞 UI）。
fn begin_preview_nav(state: &mut State, meta: &EntryMeta) {
    state.preview_gen = state.preview_gen.wrapping_add(1);
    state.preview_seq = state.preview_seq.wrapping_add(1);
    state.preview_hires_requested = false;
    state.preview_loading = true;
    state.preview = Some(preview_placeholder(meta, &state.thumb_cache));
}

/// 后台解码 / 缓存命中（逻辑线程，不碰 slint::Image）。
fn dispatch_preview_load(state: &mut State, deps: &LogicDeps, meta: &EntryMeta) {
    let gen = state.preview_gen;
    match meta.kind {
        EntryKind::Image | EntryKind::Files => {
            if meta.kind == EntryKind::Image {
                if let Some(p) = state.preview_cache.get(&meta.id).cloned() {
                    emit_cached_preview(deps, gen, meta.id, p);
                    return;
                }
            }
            state.preview_loader.request(PreviewReq {
                gen,
                meta: meta.clone(),
                settings: state.settings.clone(),
                file_idx: 0,
                file_images: Vec::new(),
                decode_width: PREVIEW_DECODE_WIDTH,
            });
        }
        _ => {
            state.preview_loading = false;
            state.preview = Some(load_preview(
                meta,
                &deps.store,
                &state.settings,
                &deps.rendition,
                PREVIEW_DECODE_WIDTH,
            ));
        }
    }
}

fn emit_cached_preview(deps: &LogicDeps, gen: u64, id: i64, p: PreviewData) {
    let _ = deps.evt_tx.send(AppEvt::PreviewReady {
        gen,
        id,
        tier: 2,
        has_image: p.has_image,
        image: p.image,
        text: p.text,
        info: p.info,
        mono: p.mono,
        file_images: p.file_images,
        file_image_idx: p.file_image_idx,
    });
}

/// 滚轮放大超过阈值后，用同一 preview_gen 再解一版高清（不 bump seq，zoom 保持）。
fn request_preview_hires(state: &mut State, zoom: f32) {
    if zoom < PREVIEW_HIRES_ZOOM || !state.preview_open || state.preview_hires_requested {
        return;
    }
    let Some(meta) = state.items.get(state.selected).cloned() else {
        return;
    };
    if meta.kind != EntryKind::Image && meta.kind != EntryKind::Files {
        return;
    }
    let Some(p) = state.preview.as_ref() else {
        return;
    };
    if !p.has_image {
        return;
    }
    if p.image.w >= PREVIEW_ZOOM_WIDTH {
        state.preview_hires_requested = true;
        return;
    }
    state.preview_hires_requested = true;
    state.preview_loader.request(PreviewReq {
        gen: state.preview_gen,
        meta,
        settings: state.settings.clone(),
        file_idx: p.file_image_idx,
        file_images: p.file_images.clone(),
        decode_width: PREVIEW_ZOOM_WIDTH,
    });
}

/// 预览请求（小结构，channel 里排队无压力）。
struct PreviewReq {
    gen: u64,
    meta: EntryMeta,
    settings: Settings,
    /// 多图文件当前要解的下标；单图忽略。
    file_idx: usize,
    /// 非空则 worker 不再查 sqlite 文件列表（切图路径）。
    file_images: Vec<String>,
    /// 解码目标宽度：首屏 520，滚轮放大 1600。
    decode_width: u32,
}

#[derive(Clone)]
struct PreviewLoader {
    tx: std::sync::mpsc::Sender<PreviewReq>,
}

impl PreviewLoader {
    fn request(&self, req: PreviewReq) {
        let _ = self.tx.send(req);
    }
}

/// 单预览 worker：同一时刻只有一个解码在跑。
/// 首屏 520；放大请求 `decode_width=1600`。邻居预取始终 520。
fn preview_worker(
    rx: std::sync::mpsc::Receiver<PreviewReq>,
    store: Store,
    evt_tx: std::sync::mpsc::Sender<AppEvt>,
    rdir: std::path::PathBuf,
    rendition: crate::preview_rendition::RenditionQueue,
) {
    loop {
        let mut req = match rx.recv() {
            Ok(r) => r,
            Err(_) => break,
        };
        'nav: loop {
            while let Ok(newer) = rx.try_recv() {
                req = newer;
            }
            let hires = req.decode_width > PREVIEW_DECODE_WIDTH;
            if req.meta.kind == EntryKind::Files {
                let Some(images) = preview_send_file(&req, &store, &evt_tx) else {
                    break;
                };
                let idx = req.file_idx.min(images.len().saturating_sub(1));
                let n = images.len() as i32;
                // 高清只解当前图；邻居仍按 520 预取，避免放大时内存和 IO 爆炸。
                if !hires && n > 1 {
                    for delta in [1i32, -1] {
                        if let Ok(newer) = rx.try_recv() {
                            req = newer;
                            continue 'nav;
                        }
                        let nidx = (idx as i32 + delta).rem_euclid(n) as usize;
                        if nidx != idx {
                            preview_send_file_at(&req, &images, nidx, PREVIEW_DECODE_WIDTH, &evt_tx);
                        }
                    }
                }
                break;
            }
            // JPEG 命中即出图（放大时解到 1280 原宽，不再压到 520）。
            if preview_tier2(&req, &store, &evt_tx, &rdir) {
                break;
            }
            preview_tier3(&req, &store, &evt_tx, &rendition);
            break;
        }
    }
}

/// Tier2：磁盘渲染图（1280 JPEG，小文件快解）先上图。返回是否已出图。
fn preview_tier2(
    req: &PreviewReq,
    store: &Store,
    evt_tx: &std::sync::mpsc::Sender<AppEvt>,
    rdir: &std::path::Path,
) -> bool {
    if req.meta.kind != EntryKind::Image {
        return false;
    }
    let Some(jpg) = crate::preview_rendition::load(rdir, req.meta.id) else {
        return false;
    };
    let mid = decode_image_limited(&jpg, req.decode_width);
    if mid.w == 0 {
        return false;
    }
    let p = image_preview_data(&req.meta, store, mid, req.meta.preview.clone());
    let _ = evt_tx.send(AppEvt::PreviewReady {
        gen: req.gen,
        id: req.meta.id,
        tier: preview_ready_tier(req.decode_width),
        has_image: p.has_image,
        image: p.image,
        text: p.text,
        info: p.info,
        mono: p.mono,
        file_images: p.file_images,
        file_image_idx: p.file_image_idx,
    });
    true
}

/// Tier3：全解精化（放大/200% DPI 用），结果进内存缓存。
fn preview_tier3(
    req: &PreviewReq,
    store: &Store,
    evt_tx: &std::sync::mpsc::Sender<AppEvt>,
    rendition: &crate::preview_rendition::RenditionQueue,
) {
    let p = load_preview(
        &req.meta,
        store,
        &req.settings,
        rendition,
        req.decode_width,
    );
    let _ = evt_tx.send(AppEvt::PreviewReady {
        gen: req.gen,
        id: req.meta.id,
        tier: preview_ready_tier(req.decode_width),
        has_image: p.has_image,
        image: p.image,
        text: p.text,
        info: p.info,
        mono: p.mono,
        file_images: p.file_images,
        file_image_idx: p.file_image_idx,
    });
}

fn preview_placeholder(_meta: &EntryMeta, _thumbs: &HashMap<i64, ImageData>) -> PreviewData {
    PreviewData {
        has_image: false,
        image: ImageData::default(),
        text: String::new(),
        info: "加载中…".into(),
        mono: false,
        file_images: Vec::new(),
        file_image_idx: 0,
    }
}

fn preview_ready_tier(decode_width: u32) -> u8 {
    if decode_width > PREVIEW_DECODE_WIDTH {
        4
    } else {
        2
    }
}

fn load_preview(
    meta: &EntryMeta,
    store: &Store,
    settings: &Settings,
    rendition: &crate::preview_rendition::RenditionQueue,
    decode_width: u32,
) -> PreviewData {
    if let Some(idx) = phrase_index_of(meta.id) {
        let content = settings
            .phrases
            .get(idx)
            .map(|p| p.content.clone())
            .unwrap_or_default();
        let n = content.chars().count();
        return preview_text_only(content, format!("快捷短语 · {n} 字"));
    }
    match meta.kind {
        EntryKind::Text => {
            let full = store.get_text(meta.id).unwrap_or_default();
            let n = full.chars().count();
            preview_text_only(full, format!("文本 · {n} 字"))
        }
        EntryKind::Image => {
            let (image, dims) = match store.get_image(meta.id) {
                Some(row) => {
                    let img = decode_image_limited(&row.blob, decode_width);
                    // 渲染图交给 rendition worker 落盘（预览 worker 只管显示，不编码）。
                    rendition.request(meta.id);
                    (img, format!("图片 {}×{}", row.w, row.h))
                }
                None => (ImageData::default(), "图片".to_string()),
            };
            image_preview_data(meta, store, image, dims)
        }
        EntryKind::Files => load_files_preview(meta, store, decode_width),
        EntryKind::RichText => {
            let full = store.get_text(meta.id).unwrap_or_default();
            let n = full.chars().count();
            let body = if full.trim().is_empty() {
                store.get_html(meta.id).unwrap_or_default()
            } else {
                full
            };
            preview_text_only(body, format!("富文本 · {n} 字 · 粘贴还原格式"))
        }
    }
}

fn image_preview_data(
    meta: &EntryMeta,
    store: &Store,
    image: ImageData,
    dims: String,
) -> PreviewData {
    let ocr = store.get_ocr(meta.id);
    let ocr_state = ocr.as_ref().map(|o| o.state).unwrap_or(0);
    let ocr_text = ocr.and_then(|o| o.text).unwrap_or_default();
    let info = match ocr_state {
        2 if ocr_text.trim().is_empty() => format!("{dims} · OCR：未识别到文字"),
        2 => format!("{dims} · OCR 文本"),
        3 => format!("{dims} · OCR 失败"),
        _ => format!("{dims} · OCR 进行中…"),
    };
    PreviewData {
        has_image: true,
        image,
        text: ocr_text,
        info,
        mono: false,
        file_images: Vec::new(),
        file_image_idx: 0,
    }
}

fn preview_text_only(text: String, info: String) -> PreviewData {
    let (text, mono, extra) = format_preview_text(text);
    let info = match extra {
        Some(s) => format!("{info} · {s}"),
        None => info,
    };
    PreviewData {
        has_image: false,
        image: ImageData::default(),
        text,
        info,
        mono,
        file_images: Vec::new(),
        file_image_idx: 0,
    }
}

/// 预览正文上限：避免超大 JSON pretty-print 撑爆 UI。
const PREVIEW_TEXT_MAX: usize = 48_000;

fn truncate_preview_text(s: &str) -> String {
    let mut it = s.chars();
    let taken: String = it.by_ref().take(PREVIEW_TEXT_MAX).collect();
    if it.next().is_some() {
        format!("{taken}\n…（已截断）")
    } else {
        taken
    }
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Object(_) => "对象",
        serde_json::Value::Array(_) => "数组",
        serde_json::Value::String(_) => "字符串",
        serde_json::Value::Number(_) => "数字",
        serde_json::Value::Bool(_) => "布尔",
        serde_json::Value::Null => "null",
    }
}

fn json_shape_label(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&str> = m.keys().map(|k| k.as_str()).take(8).collect();
            let extra = m.len().saturating_sub(keys.len());
            if extra > 0 {
                keys.push("…");
            }
            format!("JSON 对象 · {} 个键（{}）", m.len(), keys.join(", "))
        }
        serde_json::Value::Array(a) => {
            let inner = a.first().map(json_type_name).unwrap_or("空");
            format!("JSON 数组 · {} 项 · 元素: {inner}", a.len())
        }
        other => format!("JSON · {}", json_type_name(other)),
    }
}

/// 识别 JSON 则 pretty-print，便于看层级；否则原样展示。
fn format_preview_text(raw: String) -> (String, bool, Option<String>) {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let extra = json_shape_label(&v);
            let pretty = serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.clone());
            return (truncate_preview_text(&pretty), true, Some(extra));
        }
    }
    (truncate_preview_text(&raw), false, None)
}

fn load_files_preview(meta: &EntryMeta, store: &Store, decode_width: u32) -> PreviewData {
    load_files_preview_at(meta, store, 0, &[], decode_width)
}

fn load_files_preview_at(
    meta: &EntryMeta,
    store: &Store,
    idx: usize,
    preloaded: &[String],
    decode_width: u32,
) -> PreviewData {
    let paths = store.get_files(meta.id).unwrap_or_default();
    let n = paths.len();
    let images: Vec<String> = if preloaded.is_empty() {
        paths
            .iter()
            .filter(|p| clipx_core::path_looks_like_image(p))
            .cloned()
            .collect()
    } else {
        preloaded.to_vec()
    };
    let text = paths.join("\n");
    if images.is_empty() {
        return PreviewData {
            has_image: false,
            image: ImageData::default(),
            text,
            info: format!("文件 · {n} 项"),
            mono: false,
            file_images: Vec::new(),
            file_image_idx: 0,
        };
    }
    let idx = idx.min(images.len() - 1);
    let image = decode_image_limited_path(&images[idx], decode_width);
    PreviewData {
        has_image: image.w > 0,
        image,
        text,
        info: format!("文件 · {n} 项 · {}/{} 图", idx + 1, images.len()),
        mono: false,
        file_images: images,
        file_image_idx: idx,
    }
}

fn preview_send_file(
    req: &PreviewReq,
    store: &Store,
    evt_tx: &std::sync::mpsc::Sender<AppEvt>,
) -> Option<Vec<String>> {
    let p = load_files_preview_at(
        &req.meta,
        store,
        req.file_idx,
        &req.file_images,
        req.decode_width,
    );
    let images = p.file_images.clone();
    send_preview_ready(req, evt_tx, preview_ready_tier(req.decode_width), p);
    if images.is_empty() {
        None
    } else {
        Some(images)
    }
}

fn preview_send_file_at(
    req: &PreviewReq,
    images: &[String],
    idx: usize,
    decode_width: u32,
    evt_tx: &std::sync::mpsc::Sender<AppEvt>,
) {
    if images.is_empty() {
        return;
    }
    let idx = idx.min(images.len() - 1);
    let image = decode_image_limited_path(&images[idx], decode_width);
    let _ = evt_tx.send(AppEvt::PreviewReady {
        gen: req.gen,
        id: req.meta.id,
        tier: preview_ready_tier(decode_width),
        has_image: image.w > 0,
        image,
        text: String::new(),
        info: String::new(),
        mono: false,
        file_images: images.to_vec(),
        file_image_idx: idx,
    });
}

fn send_preview_ready(
    req: &PreviewReq,
    evt_tx: &std::sync::mpsc::Sender<AppEvt>,
    tier: u8,
    p: PreviewData,
) {
    let _ = evt_tx.send(AppEvt::PreviewReady {
        gen: req.gen,
        id: req.meta.id,
        tier,
        has_image: p.has_image,
        image: p.image,
        text: p.text,
        info: p.info,
        mono: p.mono,
        file_images: p.file_images,
        file_image_idx: p.file_image_idx,
    });
}

/// 预览打开且多图文件：←→ 切图。返回 true 表示已消费，不再翻页。
fn preview_step_image(
    state: &mut State,
    _deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    delta: i32,
) -> bool {
    if !state.preview_open {
        return false;
    }
    let Some(p) = state.preview.as_ref() else {
        return false;
    };
    if p.file_images.len() <= 1 {
        return false;
    }
    let len = p.file_images.len() as i32;
    let next = (p.file_image_idx as i32 + delta).rem_euclid(len) as usize;
    let path = p.file_images[next].clone();
    let paths = p.file_images.clone();
    let n_files = p.text.lines().count();
    let info = format!("文件 · {n_files} 项 · {}/{} 图", next + 1, paths.len());
    let cached = state.file_preview_cache.get(&path).cloned();
    let decode_width = if state.preview_hires_requested {
        PREVIEW_ZOOM_WIDTH
    } else {
        PREVIEW_DECODE_WIDTH
    };

    {
        let p = state.preview.as_mut().unwrap();
        p.file_image_idx = next;
        p.info = info;
        if let Some(img) = cached.as_ref() {
            p.image = img.clone();
            p.has_image = p.image.w > 0;
        }
    }
    if cached.is_some() {
        state.preview_loading = false;
        push_preview_image(state, weak, false);
        if decode_width <= PREVIEW_DECODE_WIDTH {
            return true;
        }
        // 已放大：520 缓存先顶上，再补当前图高清。
    } else {
        state.preview_loading = true;
        push_preview_loading_shell(state, weak);
    }
    if let Some(meta) = state.items.get(state.selected).cloned() {
        state.preview_loader.request(PreviewReq {
            gen: state.preview_gen,
            meta,
            settings: state.settings.clone(),
            file_idx: next,
            file_images: paths,
            decode_width,
        });
    }
    true
}

fn remember_file_preview(state: &mut State, path: String, img: ImageData) {
    if img.w == 0 {
        return;
    }
    if state.file_preview_cache.len() >= FILE_PREVIEW_CACHE_CAP
        && !state.file_preview_cache.contains_key(&path)
    {
        state.file_preview_cache.clear();
    }
    state.file_preview_cache.insert(path, img);
}

/// 字节 → RGBA。Windows 走 WIC 边解边缩；失败再回 `image` crate。
fn decode_image_limited(bytes: &[u8], max_dim: u32) -> ImageData {
    #[cfg(windows)]
    if let Some(d) = crate::wic::decode_limited(bytes, max_dim) {
        if d.w > 0 && d.h > 0 {
            return ImageData {
                rgba: d.rgba.into(),
                w: d.w,
                h: d.h,
            };
        }
    }
    let Ok(img) = image::load_from_memory(bytes) else {
        return ImageData::default();
    };
    let img = if img.width() > max_dim {
        img.thumbnail(max_dim, u32::MAX)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    ImageData {
        rgba: rgba.into_raw().into(),
        w,
        h,
    }
}

fn decode_image_limited_path(path: &str, max_dim: u32) -> ImageData {
    #[cfg(windows)]
    if let Some(d) = crate::wic::decode_limited_path(path, max_dim) {
        if d.w > 0 && d.h > 0 {
            return ImageData {
                rgba: d.rgba.into(),
                w: d.w,
                h: d.h,
            };
        }
    }
    std::fs::read(path)
        .ok()
        .map(|b| decode_image_limited(&b, max_dim))
        .unwrap_or_default()
}

/// 事件循环线程上调用：RGBA → slint::Image。
fn to_slint_image(d: ImageData) -> slint::Image {
    if d.w == 0 || d.h == 0 {
        return slint::Image::default();
    }
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(d.w, d.h);
    let dst = buf.make_mut_slice();
    let nbytes = dst.len() * 4;
    if d.rgba.len() != nbytes {
        return slint::Image::default();
    }
    unsafe {
        std::ptr::copy_nonoverlapping(d.rgba.as_ptr(), dst.as_mut_ptr().cast::<u8>(), nbytes);
    }
    slint::Image::from_rgba8(buf)
}

/// 呼出时同步解开当前窗缩略图，避免第一屏空白。
fn ensure_window_thumbs(state: &mut State, deps: &LogicDeps) {
    if state.items.is_empty() {
        return;
    }
    let vis = visible_rows(state).max(1);
    let (base, end) = row_window(state.first_visible, vis, state.items.len());
    for m in &state.items[base..end] {
        if state.thumb_cache.contains_key(&m.id) {
            continue;
        }
        if let Some(t) = deps.store.get_thumb(m.id) {
            if !t.blob.is_empty() {
                state.thumb_cache.insert(m.id, decode_image_limited(&t.blob, 128));
            }
        }
    }
    prune_thumb_cache(state);
}

/// 解码缩略图缓存上限：滚完 2000 图历史会无界增长（约 70MB+，直接爆 30MB 预算），
/// 超限只保留当前窗口附近，其余丢弃（滚回时后台线程重解，不堵滚动）。
const THUMB_CACHE_CAP: usize = 320;

fn prune_thumb_cache(state: &mut State) {
    if state.thumb_cache.len() <= THUMB_CACHE_CAP {
        return;
    }
    let vis = visible_rows(state).max(1);
    let lo = state.first_visible.saturating_sub(128).min(state.items.len());
    let hi = state
        .first_visible
        .saturating_add(vis + 128)
        .min(state.items.len())
        .max(lo);
    let keep: std::collections::HashSet<i64> =
        state.items.get(lo..hi).map(|w| w.iter().map(|m| m.id).collect()).unwrap_or_default();
    state.thumb_cache.retain(|id, _| keep.contains(id));
}

/// 滚轮换窗：缺的缩略图丢到后台，不堵惯性。
fn request_window_thumbs(state: &mut State, deps: &LogicDeps) {
    if state.items.is_empty() {
        return;
    }
    let vis = visible_rows(state).max(1);
    let (base, end) = row_window(state.first_visible, vis, state.items.len());
    let mut missing = Vec::new();
    for m in &state.items[base..end] {
        if !matches!(m.kind, EntryKind::Image | EntryKind::Files) {
            continue;
        }
        if state.thumb_cache.contains_key(&m.id) {
            continue;
        }
        if !state.thumb_inflight.insert(m.id) {
            continue;
        }
        missing.push(m.id);
    }
    if missing.is_empty() {
        return;
    }
    let store = deps.store.clone();
    let tx = deps.evt_tx.clone();
    let _ = std::thread::Builder::new()
        .name("clipx-thumbs".into())
        .spawn(move || {
            let mut thumbs = Vec::with_capacity(missing.len());
            for id in missing {
                let img = store
                    .get_thumb(id)
                    .filter(|t| !t.blob.is_empty())
                    .map(|t| decode_image_limited(&t.blob, 128))
                    .unwrap_or_default();
                thumbs.push((id, img));
            }
            let _ = tx.send(AppEvt::FileThumbsReady(thumbs));
        });
}

fn show_popup(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    state.query.clear();
    state.query_dirty_at = None;
    state.last_refresh_at = Some(std::time::Instant::now());
    state.filter = None;
    state.phrase_only = false;
    state.phrase_edit = None;
    state.preview_open = false;
    state.preview = None;
    state.menu_open = false;
    state.menu_index = -1;
    state.hidden_at = None;
    state.shown_at = Some(std::time::Instant::now());
    state.items = merge_list_items(state, deps);
    select_single(state, 0);
    state.first_visible = 0;
    state.pending_delete = None;
    ensure_window_thumbs(state, deps);
    state.visible = true;
    keyboard_hook::set_visible(true);
    keyboard_hook::arm_click_modifiers();
    #[cfg(windows)]
    {
        state.foreground_at_show = win_popup::foreground_hwnd();
    }
    let mut bundle = ui_bundle(state);
    bundle.show = true;
    bundle.reposition = true;
    #[cfg(windows)]
    let (ax, ay, anchor_branch) =
        win_popup::resolve_popup_anchor(&state.settings.popup_position);
    #[cfg(not(windows))]
    let (ax, ay, anchor_branch) = (0, 0, "unsupported".to_string());
    bundle.anchor_x = ax;
    bundle.anchor_y = ay;
    #[cfg(windows)]
    win_popup::append_pos_log(&format!(
        "show mode={} {} branch={} {}",
        state.settings.popup_position,
        win_popup::fg_debug(),
        anchor_branch,
        win_popup::placement_debug(bundle.width, bundle.height, ax, ay),
    ));
    invoke_ui(weak, bundle);
    let store = deps.store.clone();
    let tx = deps.evt_tx.clone();
    let _ = std::thread::Builder::new()
        .name("clipx-file-thumbs".into())
        .spawn(move || {
            let rows = store.backfill_file_thumbs(40);
            let thumbs: Vec<(i64, ImageData)> = rows
                .into_iter()
                .map(|(id, t)| (id, decode_image_limited(&t.blob, 128)))
                .filter(|(_, img)| img.w > 0)
                .collect();
            if !thumbs.is_empty() {
                let _ = tx.send(AppEvt::FileThumbsReady(thumbs));
            }
        });
}

fn hide_popup(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    state.visible = false;
    state.query_dirty_at = None;
    state.preview_open = false;
    state.preview = None;
    state.preview_hires_requested = false;
    state.preview_loading = false;
    state.file_preview_cache.clear();
    state.menu_open = false;
    state.menu_index = -1;
    state.phrase_edit = None;
    state.text_edit = None;
    sync_edit_chrome(state, weak);
    state.hidden_at = Some(std::time::Instant::now());
    state.shown_at = None;
    keyboard_hook::set_visible(false);
    #[cfg(windows)]
    mouse_hook::POPUP_HWND.store(0, std::sync::atomic::Ordering::SeqCst);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            crate::keyboard_hook::reinstall();
            crate::mouse_hook::reinstall();
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
    // 本次刷新已覆盖最新 query，去抖标记同步清理。
    state.query_dirty_at = None;
    state.last_refresh_at = Some(std::time::Instant::now());
    state.items = merge_list_items(state, deps);
    // 去重/触顶可能删掉旧 id，队列里失效项对齐 WPF Deduplicate* 出队。
    state.batch_queue.retain(|id| {
        is_phrase_id(*id) || state.items.iter().any(|m| m.id == *id)
    });
    if reset_selection {
        select_single(state, 0);
        state.first_visible = 0;
    } else {
        clamp_selection(state);
        ensure_selection_visible(state);
    }
    ensure_window_thumbs(state, deps);
    push_ui(state, weak);
}

/// 输入去抖刷新：距上次全量刷新超过阈值则立即刷（首字跟手），否则挂 dirty，
/// 由 200ms pump 超时分支在输入停顿后补刷。连打/组词/粘贴长串时把 N 次重建并成 1 次。
fn refresh_input(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    let now = std::time::Instant::now();
    let due = state
        .last_refresh_at
        .map(|t| now.duration_since(t).as_millis() >= INPUT_DEBOUNCE_MS)
        .unwrap_or(true);
    if due {
        refresh(state, deps, weak, true);
    } else {
        state.query_dirty_at = Some(now);
    }
}

/// 非输入按键先把挂起的查询刷了，保证导航/粘贴看到的是最新列表。
fn flush_query_dirty(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    if state.query_dirty_at.is_some() {
        refresh(state, deps, weak, true);
    }
}

/// pump 超时分支：输入停顿超过阈值才补刷，打字中不打断。
fn flush_query_dirty_aged(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    if let Some(t) = state.query_dirty_at {
        if state.visible && t.elapsed().as_millis() >= INPUT_DEBOUNCE_MS {
            refresh(state, deps, weak, true);
        }
    }
}

/// 按类型回写剪贴板（Gate 防自采在 arm 后由 monitor 吸收）。
/// 返回 false = 条目数据缺失（图片/文件读库失败），调用方不应继续。
fn write_entry_clipboard(
    meta: &EntryMeta,
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
    settings: &Settings,
    for_console: bool,
) -> bool {
    let Some(ctx) = clipboard else { return false };
    if let Some(idx) = phrase_index_of(meta.id) {
        let Some(text) = settings.phrases.get(idx).map(|p| p.content.as_str()) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        deps.gate.arm();
        return paste::write_text_for_target(ctx, text, for_console).is_ok();
    }
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
            let Some(text) = deps.store.get_text(meta.id) else {
                return false;
            };
            if for_console {
                deps.gate.arm();
                return paste::write_text_for_target(ctx, &text, true).is_ok();
            }
            // 投影 + HTML 同时写回；html 缺失时退化为纯文本
            if let Some(html) = deps.store.get_html(meta.id) {
                deps.gate.arm();
                return paste::write_rich_text(ctx, &text, &html).is_ok();
            }
            deps.gate.arm();
            paste::write_text(ctx, &text).is_ok()
        }
        EntryKind::Text => {
            let Some(text) = deps.store.get_text(meta.id) else {
                return false;
            };
            deps.gate.arm();
            paste::write_text_for_target(ctx, &text, for_console).is_ok()
        }
    }
}

fn activate_item(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    if state.settings.batch_mode != "Off" {
        batch_enqueue(state, deps, weak, clipboard, idx);
    } else {
        do_paste(state, deps, weak, clipboard, idx);
    }
}

fn do_paste(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    if state
        .last_paste_at
        .map(|t| t.elapsed() < Duration::from_millis(350))
        .unwrap_or(false)
    {
        return;
    }
    let Some(meta) = state.items.get(idx) else {
        return;
    };
    let target = {
        #[cfg(windows)]
        {
            state.foreground_at_show
        }
        #[cfg(not(windows))]
        {
            0
        }
    };
    let for_console = paste::is_console_target(target);
    if !write_entry_clipboard(meta, deps, clipboard, &state.settings, for_console) {
        return;
    }
    // 粘贴触顶（对齐 WPF TouchCopiedTime + TryUpdateCopiedAt）：快捷短语不在库中，跳过。
    let meta_id = meta.id;
    touch_pasted(state, deps, meta_id);

    state.last_paste_at = Some(std::time::Instant::now());
    let pinned = state.window_pinned;
    if !pinned {
        hide_popup(state, weak);
    } else if state.settings.paste_touch_top && !is_phrase_id(meta_id) {
        // 钉住不关：刷新使触顶可见，选中跟随被粘贴条目（按下次呼出 selected=0 等价）。
        refresh(state, deps, weak, false);
        if let Some(pos) = state.items.iter().position(|m| m.id == meta_id) {
            select_single(state, pos);
            ensure_selection_visible(state);
            push_ui(state, weak);
        }
    }
    if state.settings.paste_simulate {
        // hide 走 UI 线程，先让出一轮再抢回呼出时的目标窗，否则 Ctrl+V 打到空处
        std::thread::sleep(Duration::from_millis(80));
        #[cfg(windows)]
        win_popup::restore_foreground(target);
        std::thread::sleep(Duration::from_millis(30));
        let mode = paste::paste_mode_for_target(target, &state.settings.paste_mode);
        paste::send_paste(mode);
    }
}

fn cycle_batch_mode(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) {
    state.settings.batch_mode = match state.settings.batch_mode.as_str() {
        "Off" => "Lifo".to_string(),
        "Lifo" => "Fifo".to_string(),
        _ => "Off".to_string(),
    };
    if state.settings.batch_mode == "Off" {
        state.batch_queue.clear();
    }
    let _ = crate::settings::save(&deps.settings_path, &state.settings);
    sync_batch_watch(state);
    refresh_tray(state, deps);
    if state.visible {
        push_ui(state, weak);
    }
}

fn batch_enqueue(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    let Some(meta) = state.items.get(idx).cloned() else {
        return;
    };
    let id = meta.id;
    // 对齐 WPF SchedulePushBatchQueueHeadIfChanged：记下队首，队首不变不写剪贴板。
    let head_before = state.batch_queue.first().copied();
    state.batch_queue.retain(|qid| *qid != id);
    if state.settings.batch_mode == "Fifo" {
        state.batch_queue.push(id);
    } else {
        state.batch_queue.insert(0, id);
    }
    if state.batch_queue.first().copied() != head_before {
        if let Some(head) = state.batch_queue.first().copied() {
            if batch_head_still(state, head) {
                if let Some(meta) = meta_by_id(state, deps, head) {
                    let _ = write_entry_clipboard(&meta, deps, clipboard, &state.settings, false);
                }
            }
        }
    }
    sync_batch_watch(state);
    // 入队即刷新：队列置顶重排 + 角标（对齐 WPF ReorderAllItemsQueueFirst），选中跟随入队条目。
    refresh(state, deps, weak, false);
    if let Some(pos) = state.items.iter().position(|m| m.id == id) {
        select_single(state, pos);
        ensure_selection_visible(state);
        push_ui(state, weak);
    }
}

fn batch_advance(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    if state.settings.batch_mode == "Off" || state.batch_queue.is_empty() {
        return;
    }
    let done = state.batch_queue.remove(0);
    touch_pasted(state, deps, done);
    // 面板隐藏时 refresh 不跑，队列可能含已删条目：先剪掉失效队首。
    while let Some(head) = state.batch_queue.first().copied() {
        if meta_by_id(state, deps, head).is_none() {
            state.batch_queue.remove(0);
        } else {
            break;
        }
    }
    if state.batch_queue.is_empty() {
        if state.settings.batch_auto_off_when_empty {
            state.settings.batch_mode = "Off".to_string();
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
        }
        sync_batch_watch(state);
        refresh_tray(state, deps);
        if state.visible {
            refresh(state, deps, weak, false);
        }
        return;
    }
    let head = state.batch_queue[0];
    let mut wrote = false;
    if batch_head_still(state, head) {
        if let Some(meta) = meta_by_id(state, deps, head) {
            wrote = write_entry_clipboard(&meta, deps, clipboard, &state.settings, false);
        }
    }
    if !wrote {
        // 对齐 WPF PasteBatchQueueHeadAsync 失败恢复：下一队首写失败则把刚出队项插回队首。
        state.batch_queue.insert(0, done);
        sync_batch_watch(state);
        if state.visible {
            refresh(state, deps, weak, false);
        }
        return;
    }
    sync_batch_watch(state);
    if state.visible {
        refresh(state, deps, weak, false);
    }
}

/// 对齐 WPF `BatchQueueHeadStillThisEntry`：写队首前校验模式仍在、队列非空、队首仍是预期条目，
/// 避免写回盖住用户刚复制的内容。
fn batch_head_still(state: &State, expect: i64) -> bool {
    state.settings.batch_mode != "Off" && state.batch_queue.first().copied() == Some(expect)
}

fn sync_batch_watch(state: &State) {
    crate::keyboard_hook::set_batch_watch(
        state.settings.batch_mode != "Off" && !state.batch_queue.is_empty(),
    );
}

fn meta_by_id(state: &State, deps: &LogicDeps, id: i64) -> Option<EntryMeta> {
    state
        .items
        .iter()
        .find(|m| m.id == id)
        .cloned()
        .or_else(|| {
            deps.store
                .search("", None, list_limit(&state.settings))
                .into_iter()
                .find(|m| m.id == id)
        })
}

fn list_limit(s: &Settings) -> i64 {
    s.max_items.clamp(10, 2000)
}

fn popup_w(s: &Settings) -> f32 {
    s.popup_width as f32
}

fn page_step(s: &State) -> i32 {
    s.settings.popup_page_items.clamp(1, 50) as i32
}

/// 缩略图预取窗口：ListView 自己懒实例化行，这里只决定解哪段 PNG。
const ROW_OVERSCAN: usize = 16;

fn row_window(first_visible: usize, vis: usize, len: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let base = first_visible.min(len.saturating_sub(1)).saturating_sub(ROW_OVERSCAN);
    let end = (first_visible + vis + ROW_OVERSCAN)
        .min(len)
        .max((base + 1).min(len));
    (base, end)
}

/// 当前列表可视行数（用于翻页夹紧首行）。
fn visible_rows(s: &State) -> usize {
    let search = !s.query.is_empty();
    let max_h = s.settings.popup_max_height as f32;
    let rh = row_h(&s.settings);
    let chrome = HEADER_H + FOOTER_H + if search { SEARCH_H } else { 0.0 };
    let list_h = (max_h - chrome)
        .min(s.items.len() as f32 * rh)
        .max(0.0);
    (list_h / rh).floor().max(1.0) as usize
}

fn clamp_first_visible(state: &mut State) {
    let vis = visible_rows(state);
    let max_first = state.items.len().saturating_sub(vis);
    if state.first_visible > max_first {
        state.first_visible = max_first;
    }
}

fn ensure_selection_visible(state: &mut State) {
    if state.items.is_empty() {
        state.first_visible = 0;
        return;
    }
    let vis = visible_rows(state).max(1);
    if state.selected < state.first_visible {
        state.first_visible = state.selected;
    } else if state.selected >= state.first_visible + vis {
        state.first_visible = state.selected + 1 - vis;
    }
    clamp_first_visible(state);
}

/// WPF ScrollPage：滚动窗口并保持选中项在窗口内的相对位置，序号随首行重算。
fn scroll_page(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    direction: i32,
) {
    let n = state.items.len();
    if n == 0 {
        return;
    }
    let vis = visible_rows(state).max(1);
    let rel = state.selected.saturating_sub(state.first_visible);
    let max_first = n.saturating_sub(vis);
    let new_first = (state.first_visible as i32 + direction * page_step(state))
        .clamp(0, max_first as i32) as usize;
    state.first_visible = new_first;
    select_single(state, (new_first + rel).min(n - 1));
    state.pending_delete = None;
    reload_preview_if_open(state, deps);
    push_ui(state, weak);
}

/// 可见窗口内第 1–9 项的显示序号（相对 `first_visible`）。
fn visible_index_label(i: usize, first_visible: usize) -> String {
    let rel = i as i32 - first_visible as i32 + 1;
    if (1..=9).contains(&rel) {
        rel.to_string()
    } else {
        String::new()
    }
}

fn index_of_display(n: u8, first_visible: usize, len: usize) -> Option<usize> {
    if n < 1 || n > 9 || len == 0 {
        return None;
    }
    let idx = first_visible + (n as usize - 1);
    (idx < len).then_some(idx)
}

fn prefer_batch_menu(state: &State) -> bool {
    state.settings.batch_mode != "Off" || !state.batch_queue.is_empty()
}

fn open_menu(state: &mut State, deps: &LogicDeps, idx: usize, keyboard: bool, batch: bool) {
    state.menu_open = true;
    state.menu_index = idx as i32;
    state.menu_batch = batch;
    state.menu_hl = 0;
    state.menu_keyboard_pending = keyboard;
    state.menu_rows_cache = build_menu_rows(state, deps);
}

fn is_well_formed_json(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty() && serde_json::from_str::<serde_json::Value>(t).is_ok()
}

fn build_menu_rows(state: &State, deps: &LogicDeps) -> Vec<(String, String, bool)> {
    if state.menu_batch {
        return vec![(
            "📋 批量粘贴（依次贴完全部队列）".into(),
            "batchall".into(),
            false,
        )];
    }
    let Some(meta) = state.items.get(state.menu_index.max(0) as usize) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    rows.push(("📋 粘贴".into(), "paste".into(), false));
    rows.push(("复制到剪贴板".into(), "copy".into(), false));

    let ocr_ok = meta.kind == EntryKind::Image
        && deps
            .store
            .get_ocr(meta.id)
            .and_then(|o| o.text)
            .is_some_and(|t| !t.trim().is_empty());
    if ocr_ok {
        rows.push(("📝 粘贴文字（OCR）".into(), "ocr".into(), false));
    }

    let text_kind = matches!(meta.kind, EntryKind::Text | EntryKind::RichText);
    let text = if text_kind {
        deps.store.get_text(meta.id).unwrap_or_default()
    } else {
        String::new()
    };
    let json = text_kind && is_well_formed_json(&text);
    if meta.kind == EntryKind::Image || (text_kind && !text.is_empty() && !json) {
        rows.push(("📁 作为文件粘贴（资源管理器）".into(), "file".into(), false));
    }
    if json {
        rows.push(("📄 粘贴为 JSON 文件（资源管理器）".into(), "json".into(), false));
    }
    if text_kind {
        rows.push(("✏️ 编辑文本".into(), "edit".into(), false));
    }
    if !is_phrase_id(meta.id) {
        rows.push((
            if meta.pinned {
                "取消置顶".into()
            } else {
                "置顶".into()
            },
            "pin".into(),
            false,
        ));
    }
    if meta.kind == EntryKind::Image {
        rows.push(("图片另存为".into(), "saveimg".into(), false));
    }
    if meta.kind == EntryKind::Files {
        rows.push(("复制路径".into(), "copypath".into(), false));
    }
    if !meta.source_app.is_empty() {
        rows.push((
            format!("只看来自 {}", meta.source_app),
            "source".into(),
            false,
        ));
    }
    rows.push((
        if is_phrase_id(meta.id) {
            "⚡ 修改快捷短语"
        } else {
            "⚡ 设为快捷短语"
        }
        .into(),
        "phrase".into(),
        false,
    ));
    rows.push(("🗑 删除".into(), "delete".into(), true));
    rows
}

fn slint_menu_rows(state: &State) -> Vec<MenuRow> {
    if !state.menu_open {
        return Vec::new();
    }
    state
        .menu_rows_cache
        .iter()
        .enumerate()
        .map(|(i, (label, action, danger))| MenuRow {
            label: label.into(),
            action: action.into(),
            danger: *danger,
            hot: i == state.menu_hl,
        })
        .collect()
}

fn menu_action_at(state: &State, hl: usize) -> Option<MenuAction> {
    parse_menu_action(state.menu_rows_cache.get(hl)?.1.as_str())
}

fn parse_menu_action(s: &str) -> Option<MenuAction> {
    Some(match s {
        "paste" => MenuAction::Paste,
        "copy" => MenuAction::Copy,
        "pin" => MenuAction::Pin,
        "delete" => MenuAction::Delete,
        "phrase" => MenuAction::Phrase,
        "edit" => MenuAction::Edit,
        "ocr" => MenuAction::OcrPaste,
        "file" => MenuAction::PasteAsFile,
        "json" => MenuAction::PasteAsJson,
        "saveimg" => MenuAction::SaveImage,
        "copypath" => MenuAction::CopyPath,
        "source" => MenuAction::FilterSource,
        "batchall" => MenuAction::BatchAll,
        _ => return None,
    })
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
    if matches!(action, MenuAction::BatchAll) {
        batch_flush_all(state, deps, weak, clipboard);
        return;
    }
    let Some(meta) = state.items.get(idx).cloned() else {
        push_ui(state, weak);
        return;
    };
    match action {
        MenuAction::Paste => {
            select_single(state, idx);
            activate_item(state, deps, weak, clipboard, idx);
        }
        // 复制：只写剪贴板（不模拟 Ctrl+V），写完收起弹窗
        MenuAction::Copy => {
            if write_entry_clipboard(&meta, deps, clipboard, &state.settings, false) {
                hide_popup(state, weak);
            } else {
                push_ui(state, weak);
            }
        }
        MenuAction::Pin => {
            if is_phrase_id(meta.id) {
                push_ui(state, weak);
                return;
            }
            let _ = deps.store.toggle_pin(meta.id);
            refresh(state, deps, weak, false);
            // 置顶浮动后选中跟随原条目
            select_single(
                state,
                state
                    .items
                    .iter()
                    .position(|m| m.id == meta.id)
                    .unwrap_or(state.selected),
            );
            ensure_selection_visible(state);
            reload_preview_if_open(state, deps);
            push_ui(state, weak);
        }
        MenuAction::Delete => {
            delete_item(state, deps, meta.id);
            refresh(state, deps, weak, false);
            reload_preview_if_open(state, deps);
        }
        MenuAction::Phrase => {
            begin_phrase_edit(state, deps, &meta, weak);
            push_ui(state, weak);
        }
        MenuAction::Edit => begin_text_edit(state, deps, &meta, weak),
        MenuAction::OcrPaste => paste_ocr(state, deps, weak, clipboard, idx),
        MenuAction::PasteAsFile => paste_as_file(state, deps, weak, clipboard, idx, false),
        MenuAction::PasteAsJson => paste_as_file(state, deps, weak, clipboard, idx, true),
        MenuAction::SaveImage => save_image_to_temp(state, deps, &meta, clipboard),
        MenuAction::CopyPath => copy_entry_path(state, deps, &meta, clipboard, weak),
        MenuAction::FilterSource => {
            if !meta.source_app.is_empty() {
                state.source_filter = Some(meta.source_app.clone());
                refresh(state, deps, weak, true);
            } else {
                push_ui(state, weak);
            }
        }
        MenuAction::BatchAll => {}
    }
}

fn check_foreground(
    state: &mut State,
    deps: &LogicDeps,
    _weak: &slint::Weak<PopupWindow>,
) {
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
    // FileJump 贴框随动：对话框移动/缩放时 Picker 平移相同差量；
    // 对话框已销毁则 Picker 跟着关闭（200ms 粒度，与 trim 同 tick）。
    fj_follow_tick(state, deps);
}

/// Picker 跟随兜底 tick（WPF 500ms timer 的轻量版，200ms 粒度）：
/// WinEvent 是实时主力；个别不发 LOCATIONCHANGE 的宿主靠这里补。
/// 对话框已死则关闭 Picker。
fn fj_follow_tick(state: &mut State, deps: &LogicDeps) {
    if !state.fj.visible || state.fj.dialog == 0 {
        return;
    }
    if !crate::win_popup::dialog_alive(state.fj.dialog) {
        fj_hide(state, deps);
        return;
    }
    let Some(rect) = crate::win_popup::dialog_rect(state.fj.dialog) else {
        return;
    };
    if state.fj.last_dlg_rect != Some(rect) {
        state.fj.last_dlg_rect = Some(rect);
        if crate::win_popup::redock_picker(state.fj.dialog) {
        } else {
            fj_hide(state, deps);
            return;
        }
    }
    if state.settings.filejump_auto_sync && state.fj.dialog != 0 {
        let fg = crate::win_popup::foreground_hwnd();
        if fg != state.fj.dialog {
            state.fj.left_dialog = true;
        } else if state.fj.left_dialog {
            state.fj.left_dialog = false;
            if let Some(path) = clipx_filejump::collectors::zorder_linked_folder(
                state.fj.dialog,
                Some(&deps.gate),
                2,
            ) {
                let kind = state.fj.kind.unwrap_or(clipx_filejump::DialogKind::System);
                state.fj.gen += 1;
                let gen = state.fj.gen;
                crate::filejump::spawn_navigate(
                    deps.evt_tx.clone(),
                    state.fj.dialog,
                    kind,
                    path,
                    state.settings.filejump_shell_inject,
                    gen,
                );
            }
            // 切回已开对话框：刷新候选
            let req = crate::filejump::CollectReq {
                gen: {
                    state.fj.gen += 1;
                    state.fj.gen
                },
                query: state.fj.query.clone(),
                favorites: state.settings.filejump_favorites.clone(),
                recent: state.settings.filejump_recent.clone(),
                everything_enabled: state.settings.filejump_everything_search,
                dialog: state.fj.dialog,
            };
            crate::filejump::spawn_collect(deps.evt_tx.clone(), deps.gate.clone(), req);
        }
    }
}
// 注意：弹窗是 WS_EX_NOACTIVATE，本来就不抢前台；不用前台变化关窗，
// 否则 show() 引起的任务栏/IME 抢焦会被当成失焦。外部点击由 mouse_hook 关，Esc 由键盘钩子关。

/// 逻辑线程产出的行数据（纯数据，Send）；RowData（含 slint::Image）
/// 在事件循环线程上组装。
struct RowSource {
    id: i32,
    kind: i32,
    icon: &'static str,
    index_label: String,
    preview: String,
    hit_pre: String,
    hit: String,
    hit_post: String,
    sub: String,
    time_ago: String,
    thumb: ImageData,
    has_thumb: bool,
    idx: i32,
    picked: bool,
    current: bool,
    pending_delete: bool,
}

struct UiBundle {
    rows: Vec<RowSource>,
    selected: i32,
    search_active: bool,
    search_text: SharedString,
    search_count: SharedString,
    filter_label: SharedString,
    item_count: SharedString,
    width: f32,
    height: f32,
    show: bool,
    preview_active: bool,
    preview_has_image: bool,
    preview_image: ImageData,
    preview_text: SharedString,
    preview_info: SharedString,
    preview_mono: bool,
    preview_seq: u32,
    preview_loading: bool,
    menu_visible: bool,
    menu_index: i32,
    menu_rows: Vec<MenuRow>,
    menu_position_keyboard: bool,
    list_width: f32,
    batch_label: SharedString,
    footer_hint: SharedString,
    /// 仅首次 show 时定位；后续刷新只 clamp，避免每次重算贴到鼠标。
    reposition: bool,
    anchor_x: i32,
    anchor_y: i32,
    opacity: f64,
    phrase_edit_open: bool,
    phrase_edit_preview: SharedString,
    phrase_edit_buffer: SharedString,
    text_edit_open: bool,
    text_edit_buffer: SharedString,
    row_height_px: f32,
    window_pinned: bool,
    first_visible: i32,
    /// 切片基址（rows[k] 对应全局行 row_base+k）与全局总行数（滚动条用）。
    row_base: i32,
    total_count: i32,
}

fn ui_bundle(state: &mut State) -> UiBundle {
    clamp_first_visible(state);
    let (preview_active, preview_has_image, preview_image, preview_text, preview_info, preview_mono) =
        match state.preview.as_ref() {
            Some(p) => (
                state.preview_open,
                p.has_image,
                p.image.clone(),
                p.text.clone().into(),
                p.info.clone().into(),
                p.mono,
            ),
            None => (
                false,
                false,
                ImageData::default(),
                SharedString::new(),
                SharedString::new(),
                false,
            ),
        };
    let menu_index = if state.menu_open {
        state.menu_index
    } else {
        -1
    };
    let (phrase_preview, phrase_buf) = match state.phrase_edit.as_ref() {
        Some(e) => (truncate_preview(&e.content, 2), e.buffer.clone()),
        None => (String::new(), String::new()),
    };
    let bundle = UiBundle {
        rows: {
            state.row_base = 0;
            state.row_end = state.items.len();
            build_rows(
                &state.items,
                &state.thumb_cache,
                &state.batch_queue,
                &state.settings,
                &state.query,
                state.selected,
                &state.sel_set,
                state.pending_delete,
                state.first_visible,
                0,
            )
        },
        total_count: state.items.len() as i32,
        row_base: state.row_base as i32,
        selected: state.selected as i32,
        search_active: !state.query.is_empty(),
        search_text: state.query.clone().into(),
        search_count: format!("{} 条结果", state.items.len()).into(),
        filter_label: filter_label(state).into(),
        item_count: if state.items.is_empty() {
            SharedString::default()
        } else {
            format!("({})", state.items.len()).into()
        },
        width: if preview_active {
            popup_w(&state.settings) + PREVIEW_W + POPUP_CHROME * 2.0
        } else {
            popup_w(&state.settings) + POPUP_CHROME * 2.0
        },
        height: {
            let auto = window_height(
                state.items.len(),
                !state.query.is_empty(),
                state.settings.popup_max_height as f32,
                row_h(&state.settings),
            ) + POPUP_CHROME * 2.0;
            if state.settings.popup_height > 0.0 {
                (state.settings.popup_height as f32 + POPUP_CHROME * 2.0)
                    .clamp(WIN_MIN_H, 900.0 + POPUP_CHROME * 2.0)
            } else {
                auto
            }
        },
        show: false,
        preview_active,
        preview_has_image,
        preview_image,
        preview_text,
        preview_info,
        preview_mono,
        preview_seq: state.preview_seq,
        preview_loading: state.preview_loading,
        menu_visible: state.menu_open,
        menu_index,
        menu_rows: slint_menu_rows(state),
        menu_position_keyboard: state.menu_keyboard_pending,
        list_width: popup_w(&state.settings).max(260.0),
        batch_label: batch_label(state).into(),
        footer_hint: footer_hint(state).into(),
        reposition: false,
        anchor_x: 0,
        anchor_y: 0,
        opacity: state.settings.popup_opacity,
        phrase_edit_open: state.phrase_edit.is_some(),
        phrase_edit_preview: phrase_preview.into(),
        phrase_edit_buffer: phrase_buf.into(),
        text_edit_open: state.text_edit.is_some(),
        text_edit_buffer: state
            .text_edit
            .as_ref()
            .map(|e| e.buffer.clone())
            .unwrap_or_default()
            .into(),
        row_height_px: row_h(&state.settings),
        window_pinned: state.window_pinned,
        first_visible: state.first_visible as i32,
    };
    state.menu_keyboard_pending = false;
    bundle
}

fn push_ui(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    invoke_ui(weak, ui_bundle(state));
}

fn slint_rows(rows: Vec<RowSource>) -> Vec<RowData> {
    rows.into_iter()
        .map(|r| {
            let thumb = if r.has_thumb {
                cached_slint_thumb(r.id as i64, &r.thumb)
            } else {
                slint::Image::default()
            };
            RowData {
                id: r.id,
                kind: r.kind,
                icon: r.icon.into(),
                index_label: r.index_label.into(),
                preview: r.preview.into(),
                hit_pre: r.hit_pre.into(),
                hit: r.hit.into(),
                hit_post: r.hit_post.into(),
                sub: r.sub.into(),
                time_ago: r.time_ago.into(),
                thumb,
                has_thumb: r.has_thumb,
                idx: r.idx,
                picked: r.picked,
                current: r.current,
                pending_delete: r.pending_delete,
            }
        })
        .collect()
}

// 事件循环线程专用的缩略图上传缓存：每刷新一次全量 Model 替换，
// 不缓存则可见缩略图每键重新 memcpy + 建纹理。id 永不复用（AUTOINCREMENT），
// 缩略图内容不变，缓存安全；超上限整体清空（极少触发，重传一次）。
thread_local! {
    static THUMB_UPLOADS: std::cell::RefCell<HashMap<i64, slint::Image>> =
        std::cell::RefCell::new(HashMap::new());
}
const THUMB_UPLOAD_CAP: usize = 512;

fn cached_slint_thumb(id: i64, d: &ImageData) -> slint::Image {
    if d.w == 0 || d.h == 0 {
        return slint::Image::default();
    }
    THUMB_UPLOADS.with(|m| {
        let mut m = m.borrow_mut();
        if let Some(img) = m.get(&id) {
            return img.clone();
        }
        if m.len() >= THUMB_UPLOAD_CAP {
            m.clear();
        }
        let img = to_slint_image(d.clone());
        m.insert(id, img.clone());
        img
    })
}

/// 只改已有行的缩略图，不替换 Model（替换会拆掉 ListView 正在滚的行）。
fn patch_row_thumbs(weak: &slint::Weak<PopupWindow>, patches: Vec<(usize, i64, ImageData)>) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let model = ui.get_rows();
        for (idx, id, img) in patches {
            let Some(mut row) = model.row_data(idx) else { continue };
            row.has_thumb = img.w > 0;
            row.thumb = cached_slint_thumb(id, &img);
            model.set_row_data(idx, row);
        }
    });
}

/// 只推列表选中 + 预览加载壳（零像素上传，选中必须跟手）。
fn push_selection_only(state: &State, weak: &slint::Weak<PopupWindow>) {
    let selected = state.selected as i32;
    let first_visible = state.first_visible as i32;
    let preview_active = state.preview_open;
    let loading = state.preview_loading;
    let seq = state.preview_seq as i32;
    let info = state
        .preview
        .as_ref()
        .map(|p| p.info.as_str())
        .unwrap_or("加载中…")
        .into();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_selected_index(selected);
        ui.set_first_visible(first_visible);
        if preview_active {
            ui.set_preview_active(true);
            ui.set_preview_loading(loading);
            ui.set_preview_has_image(false);
            ui.set_preview_image(slint::Image::default());
            ui.set_preview_text(SharedString::new());
            ui.set_preview_info(info);
            ui.set_preview_mono(false);
            ui.set_preview_seq(seq);
        }
    });
}

/// 方向键同页：预览开着时走 push_selection_only；否则全量推。
fn push_preview_ui(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    if state.preview_open {
        push_selection_only(state, weak);
    } else {
        push_ui(state, weak);
    }
}

/// 只刷新预览区加载壳（多图切图等，不改列表选中）。
fn push_preview_loading_shell(state: &State, weak: &slint::Weak<PopupWindow>) {
    let loading = state.preview_loading;
    let seq = state.preview_seq as i32;
    let info = state
        .preview
        .as_ref()
        .map(|p| p.info.as_str())
        .unwrap_or("加载中…")
        .into();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_preview_loading(loading);
        ui.set_preview_has_image(false);
        ui.set_preview_image(slint::Image::default());
        ui.set_preview_info(info);
        ui.set_preview_seq(seq);
    });
}

/// 预览像素就绪：下一拍再上传，让选中那一帧先画出来。
fn push_preview_image(state: &mut State, weak: &slint::Weak<PopupWindow>, reset_zoom: bool) {
    let seq = state.preview_seq as i32;
    let loading = state.preview_loading;
    let (has_image, image, text, info, mono) = match state.preview.as_ref() {
        Some(p) => (
            p.has_image,
            p.image.clone(),
            p.text.clone().into(),
            p.info.clone().into(),
            p.mono,
        ),
        None => (
            false,
            ImageData::default(),
            SharedString::new(),
            SharedString::new(),
            false,
        ),
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_preview_seq() != seq {
                return;
            }
            ui.set_preview_has_image(has_image);
            ui.set_preview_image(to_slint_image(image));
            ui.set_preview_text(text);
            ui.set_preview_info(info);
            ui.set_preview_mono(mono);
            ui.set_preview_loading(loading);
            if reset_zoom {
                ui.set_preview_seq(seq);
            }
        });
    });
}

fn invoke_ui(weak: &slint::Weak<PopupWindow>, bundle: UiBundle) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        if bundle.show {
            // 先复活钩子：上一次全量推行若寨住主线程，系统可能已静默摘钩。
            // 必须在事件循环线程执行（LL 钩子与安装线程绑定）。
            crate::keyboard_hook::reinstall();
            crate::mouse_hook::reinstall();
        }
        let t0 = std::time::Instant::now();
        let rows = slint_rows(bundle.rows);
        let nrows = rows.len();
        // 选中必须最先改，再动预览像素 / 列表行。
        ui.set_selected_index(bundle.selected);
        ui.set_first_visible(bundle.first_visible);
        if bundle.preview_active {
            ui.set_preview_loading(bundle.preview_loading);
            ui.set_preview_has_image(!bundle.preview_loading && bundle.preview_has_image);
            ui.set_preview_image(if bundle.preview_loading {
                slint::Image::default()
            } else {
                to_slint_image(bundle.preview_image)
            });
            ui.set_preview_text(bundle.preview_text);
            ui.set_preview_info(bundle.preview_info);
            ui.set_preview_mono(bundle.preview_mono);
            ui.set_preview_seq(bundle.preview_seq as i32);
            ui.set_preview_active(true);
        }
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_search_active(bundle.search_active);
        ui.set_search_text(bundle.search_text);
        ui.set_search_count(bundle.search_count);
        ui.set_type_filter_label(bundle.filter_label.into());
        ui.set_item_count(bundle.item_count);
        ui.set_batch_label(bundle.batch_label);
        ui.set_footer_hint(bundle.footer_hint);
        ui.set_list_width_px(bundle.list_width);
        ui.set_menu_visible(bundle.menu_visible);
        ui.set_menu_index(bundle.menu_index);
        ui.set_menu_rows(ModelRc::new(VecModel::from(bundle.menu_rows)));
        let phrase_was = ui.get_phrase_edit_open();
        let text_was = ui.get_text_edit_open();
        ui.set_phrase_edit_open(bundle.phrase_edit_open);
        ui.set_phrase_edit_preview(bundle.phrase_edit_preview);
        if bundle.phrase_edit_open != phrase_was {
            ui.set_phrase_edit_buffer(bundle.phrase_edit_buffer);
        }
        ui.set_text_edit_open(bundle.text_edit_open);
        if bundle.text_edit_open != text_was {
            ui.set_text_edit_buffer(bundle.text_edit_buffer);
        }
        ui.set_window_pinned(bundle.window_pinned);
        ui.set_row_height_px(bundle.row_height_px);
        ui.set_row_base(bundle.row_base);
        ui.set_total_count(bundle.total_count);
        ui.set_panel_opacity(bundle.opacity.clamp(0.4, 1.0) as f32);
        ui.set_first_visible(bundle.first_visible);
        if bundle.menu_position_keyboard && bundle.menu_index >= 0 {
            ui.invoke_menu_position_at(bundle.menu_index);
        }
        let win = ui.window();
        let w = bundle.width;
        // 呼出重定位时不要先按窗口当前（往往是主屏）scale 写 Logical 尺寸：
        // show 会按这套逻辑尺寸把 HWND 拉回主屏左上，随后的物理定位就被冲掉。
        if !win_popup::is_resizing() && !(bundle.show && bundle.reposition) {
            win.set_size(WindowSize::Logical(LogicalSize::new(w, bundle.height)));
        }
        if bundle.show {
            #[cfg(windows)]
            let scale_pre = win_popup::scale_now(win);
            // 钩子必须在 show 前装上，才能拦住 show/DPI 把窗口拽到 (0,0)。
            win_popup::ensure_resize_hook(win);
            if bundle.reposition {
                // show 前先按锚点屏 DPI 定好尺寸与初始位置：winit 首帧即落对，
                // 不在主屏左上闪一下再挪（对齐 WPF ShowPopup 首定位）。
                win_popup::lock_placement(true);
                win_popup::position_at(win, w, bundle.height, bundle.anchor_x, bundle.anchor_y);
            }
            let _ = win.show();
            if bundle.reposition {
                // show 后二次确认：show 会重置 EXSTYLE/尺寸，再定一次位
                //（对齐 WPF ShowPopup 次定位 + ApplyPendingPositionSetWindowPos）。
                win_popup::position_at(win, w, bundle.height, bundle.anchor_x, bundle.anchor_y);
                win_popup::apply_style(win);
                // apply_style 的 SetWindowPos(SWP_NOMOVE) 若赶上 HWND 还在 (0,0)，
                // 会把错误位置锁住；样式之后再钉一次物理坐标。
                win_popup::position_at(win, w, bundle.height, bundle.anchor_x, bundle.anchor_y);
                win_popup::store_hwnd(win);
                win_popup::ensure_resize_hook(win);
                win_popup::lock_placement(false);
                #[cfg(windows)]
                win_popup::append_pos_log(&format!(
                    "shown anchor=({},{}) scale_pre={scale_pre:.2} scale_post={:.2}",
                    bundle.anchor_x,
                    bundle.anchor_y,
                    win_popup::scale_now(win),
                ));
            }
        } else if !win_popup::is_resizing() {
            win_popup::clamp_to_work_area(win, w, bundle.height);
        }
        win_popup::ensure_resize_hook(win);
        // 主线程推送耗时：全量行 + 图片上传；持续超 LowLevelHooksTimeout 量级
        // 即有摘钩风险（列表虚拟化的数据依据）。
        let ms = t0.elapsed().as_millis();
        if bundle.show || ms > 250 {
            #[cfg(windows)]
            win_popup::append_debug_log(
                "hotkey_debug.log",
                &format!("ui-push rows={nrows} show={} {ms}ms", bundle.show),
            );
        }
    });
}

fn build_rows(
    items: &[EntryMeta],
    thumbs: &HashMap<i64, ImageData>,
    queue: &[i64],
    settings: &Settings,
    query: &str,
    current: usize,
    sel_set: &BTreeSet<usize>,
    pending: Option<i64>,
    first_visible: usize,
    // 切片基址：items[j] 的全局行号 = base+j（序号/选中/高亮全按全局算）。
    base: usize,
) -> Vec<RowSource> {
    let now = now_ms();
    // 查询小写串每刷新只算一次（split_hit 每行复用）。
    let ql = query.trim().to_lowercase();
    items
        .iter()
        .enumerate()
        .map(|(j, m)| {
            let i = base + j;
            let qpos = queue.iter().position(|id| *id == m.id);
            let index_label = visible_index_label(i, first_visible);
            let mut sub = kind_sub(m, settings);
            let mut preview_src = m.preview.clone();
            if m.kind == EntryKind::Files {
                let (names, n) = clipx_core::files_list_parts(&m.preview);
                preview_src = names;
                if n > 1 {
                    let tag = format!("{n} 个文件");
                    sub = if sub.is_empty() {
                        tag
                    } else {
                        format!("{tag} · {sub}")
                    };
                }
            }
            if let Some(qpos) = qpos {
                if !sub.is_empty() {
                    sub.push_str(" · ");
                }
                sub.push_str(&format!("队列 {}", qpos + 1));
            }
            let preview = truncate_preview(&preview_src, settings.preview_max_lines);
            let (hit_pre, hit, hit_post) = split_hit(&preview, &ql);
            let near = i.abs_diff(first_visible) <= ROW_OVERSCAN + 32;
            let thumb = if near {
                thumbs.get(&m.id).cloned().unwrap_or_default()
            } else {
                ImageData::default()
            };
            let has_thumb = near && thumbs.get(&m.id).is_some_and(|t| t.w > 0);
            RowSource {
                id: m.id as i32,
                kind: if is_phrase_id(m.id) {
                    4
                } else {
                    m.kind.as_i64() as i32
                },
                icon: if is_phrase_id(m.id) {
                    "⚡"
                } else {
                    kind_icon(m.kind)
                },
                index_label,
                preview,
                hit_pre,
                hit,
                hit_post,
                sub,
                time_ago: if is_phrase_id(m.id) {
                    String::new()
                } else {
                    time_ago(m.created_ms, now)
                },
                thumb,
                has_thumb,
                idx: i as i32,
                picked: row_picked(current, sel_set, i),
                current: i == current,
                pending_delete: pending == Some(m.id),
            }
        })
        .collect()
}

fn kind_icon(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Text => "📝",
        EntryKind::Image => "🖼️",
        EntryKind::Files => "📁",
        EntryKind::RichText => "📝",
    }
}

fn kind_sub(meta: &EntryMeta, settings: &Settings) -> String {
    if let Some(idx) = phrase_index_of(meta.id) {
        return settings
            .phrases
            .get(idx)
            .map(|p| format!("短语 · {}", p.phrase))
            .unwrap_or_else(|| "短语".to_string());
    }
    let mut parts = Vec::new();
    if meta.pinned {
        parts.push("已置顶".into());
    }
    if !meta.source_app.is_empty() {
        parts.push(meta.source_app.clone());
    }
    parts.join(" · ")
}

fn filter_label(state: &State) -> String {
    if state.phrase_only {
        return "⚡ 短语".into();
    }
    if let Some(src) = state.source_filter.as_deref() {
        return format!("来源 · {src}");
    }
    match state.filter {
        None => "全部".into(),
        Some(EntryKind::Text | EntryKind::RichText) => "📝 文本".into(),
        Some(EntryKind::Image) => "🖼️ 图片".into(),
        Some(EntryKind::Files) => "📁 文件".into(),
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

fn window_height(rows: usize, search: bool, max_h: f32, row_h: f32) -> f32 {
    let max_h = max_h.clamp(WIN_MIN_H, 900.0);
    let chrome = HEADER_H + FOOTER_H + if search { SEARCH_H } else { 0.0 };
    let content = (rows as f32 * row_h).min(max_h - chrome).max(0.0);
    (chrome + content).clamp(WIN_MIN_H, max_h)
}

fn row_h(s: &Settings) -> f32 {
    let n = s.preview_max_lines.clamp(1, 10);
    (ROW_H + (n as f32 - 1.0) * 16.0).min(92.0)
}

fn batch_label(state: &State) -> String {
    match state.settings.batch_mode.as_str() {
        "Fifo" => {
            if state.batch_queue.is_empty() {
                "FIFO".into()
            } else {
                format!("FIFO · {}", state.batch_queue.len())
            }
        }
        "Lifo" => {
            if state.batch_queue.is_empty() {
                "LIFO".into()
            } else {
                format!("LIFO · {}", state.batch_queue.len())
            }
        }
        _ => "普通".into(),
    }
}

fn footer_hint(state: &State) -> String {
    if !state.notice.is_empty() {
        return state.notice.clone();
    }
    if state.window_pinned {
        return "已钉住 · 粘贴不关 · 点图钉取消".into();
    }
    if state.pending_delete.is_some() {
        return "再按 Del 确认删除 · Esc 取消".into();
    }
    let nsel = selected_count(state);
    if nsel > 1 {
        return format!("{nsel} 项已选 · Enter连贴");
    }
    let m = match state.settings.panel_key.as_str() {
        "Alt" => "Alt",
        "Win" => "Win",
        "CapsLock" => "Caps",
        _ => "Ctrl",
    };
    format!("{m}+N快贴 · ↑↓选择 · ←→翻页 · Enter粘贴 · Space预览 · Del×2 · Alt菜单")
}

// ================= FileJump Picker (M5d) =================

fn fj_show(state: &mut State, deps: &LogicDeps, found: Option<isize>, auto_jump: bool) {
    let gen = {
        let fj = &mut state.fj;
        fj.gen += 1;
        fj.visible = true;
        fj.dialog = found.unwrap_or(0);
        fj.global_mode = found.is_none();
    fj.query.clear();
    fj.items.clear();
    fj.filtered.clear();
    fj.current_folder = None;
    fj.selected = 0;
    fj.pending_auto_jump = auto_jump && !fj.global_mode;
    fj.navigating = false;
    fj.hint = "正在采集路径…".to_string();
    fj.shown_at = Some(std::time::Instant::now());
    fj.last_dlg_rect = found.and_then(crate::win_popup::dialog_rect);
    #[cfg(windows)]
    {
        fj.follow_kill = crate::filejump::follow_spawn(deps.evt_tx.clone(), fj.dialog);
    }
    fj.gen
    };
    state.fj.kind = found.and_then(|d| {
        #[cfg(windows)]
        {
            clipx_filejump::dialog::win::classify_hwnd(d).ok()
        }
        #[cfg(not(windows))]
        {
            let _ = d;
            None
        }
    });
    keyboard_hook::set_visible(true);
    crate::keyboard_hook::set_fj_picker(true);
    crate::keyboard_hook::set_fj_dialog(state.fj.dialog);
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, true, state.fj.dialog);
    let req = crate::filejump::CollectReq {
        gen,
        query: String::new(),
        favorites: state.settings.filejump_favorites.clone(),
        recent: state.settings.filejump_recent.clone(),
        everything_enabled: false, // empty query: no Everything supplement yet
        dialog: state.fj.dialog,
    };
    crate::filejump::spawn_collect(deps.evt_tx.clone(), deps.gate.clone(), req);
}

fn fj_hide(state: &mut State, deps: &LogicDeps) {
    state.fj.visible = false;
    state.fj.gen += 1; // drop in-flight background results
    state.fj.navigating = false;
    state.fj.pending_auto_jump = false;
    state.fj.last_dlg_rect = None;
    #[cfg(windows)]
    {
        crate::filejump::follow_stop(state.fj.follow_kill);
        state.fj.follow_kill = 0;
    }
    if !state.visible {
        keyboard_hook::set_visible(false);
    }
    crate::keyboard_hook::set_fj_picker(false);
    crate::keyboard_hook::set_fj_dialog(0);
    crate::filejump::hide_fj_ui(&deps.fj);
}

fn fj_apply_filter_inner(fj: &mut crate::filejump::FjState) {
    fj.filtered = crate::filejump::filter_items(&fj.items, &fj.query);
    if fj.fav_only {
        fj.filtered.retain(|&i| {
            fj.items
                .get(i)
                .map(|c| c.source == clipx_filejump::CandidateSource::Favorite)
                .unwrap_or(false)
        });
    }
    if fj.selected >= fj.filtered.len() {
        fj.selected = fj.filtered.len().saturating_sub(1);
    }
}

fn fj_apply_filter(state: &mut State) {
    fj_apply_filter_inner(&mut state.fj);
}

fn fj_refilter(state: &mut State, deps: &LogicDeps, ev_requery: bool) {
    let gen = {
        let fj = &mut state.fj;
        fj.gen += 1;
        fj_apply_filter_inner(fj);
        fj.selected = 0;
        fj.first_visible = 0;
        fj.hint.clear();
        fj.gen
    };
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
    if ev_requery && !state.fj.query.trim().is_empty() {
        crate::filejump::spawn_ev_query(
            deps.evt_tx.clone(),
            gen,
            state.fj.query.clone(),
            state.fj.current_folder.clone(),
            state.settings.filejump_everything_search,
        );
    }
}

fn fj_activate(state: &mut State, deps: &LogicDeps) {
    let Some(&idx) = state.fj.filtered.get(state.fj.selected) else {
        return;
    };
    let Some(c) = state.fj.items.get(idx).cloned() else {
        return;
    };
    if state.fj.global_mode || state.fj.dialog == 0 {
        // No dialog: open in Explorer + learn as recent.
        #[cfg(windows)]
        clipx_filejump::inject::win::open_in_explorer(&c.path);
        let mut recent = state.settings.filejump_recent.clone();
        crate::filejump::push_recent(
            &deps.settings_path,
            &mut recent,
            &c.path,
            state.settings.filejump_recent_max,
        );
        fj_hide(state, deps);
        return;
    }
    let kind = state.fj.kind.unwrap_or(clipx_filejump::DialogKind::System);
    state.fj.navigating = true;
    state.fj.gen += 1;
    let gen = state.fj.gen;
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
    crate::filejump::spawn_navigate(
        deps.evt_tx.clone(),
        state.fj.dialog,
        kind,
        c.path,
        state.settings.filejump_shell_inject,
        gen,
    );
}

fn fj_toggle_favorite(state: &mut State, deps: &LogicDeps) {
    let Some(&idx) = state.fj.filtered.get(state.fj.selected) else {
        return;
    };
    let Some(path) = state.fj.items.get(idx).map(|c| c.path.clone()) else {
        return;
    };
    let mut favs = state.settings.filejump_favorites.clone();
    if favs
        .iter()
        .any(|f| f.path().to_lowercase() == path.to_lowercase())
    {
        favs.retain(|f| f.path().to_lowercase() != path.to_lowercase());
        state.fj.hint = "已从收藏移除".to_string();
    } else {
        favs.push(crate::settings::FolderFavorite::full(String::new(), path));
        state.fj.hint = "已加入收藏".to_string();
    }
    crate::filejump::persist_lists(
        &deps.settings_path,
        &favs,
        &state.settings.filejump_recent,
        state.settings.filejump_recent_max,
    );
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
}

fn fj_remove_selected(state: &mut State, deps: &LogicDeps) {
    use clipx_filejump::CandidateSource;
    let Some(&idx) = state.fj.filtered.get(state.fj.selected) else {
        return;
    };
    let Some(c) = state.fj.items.get(idx).cloned() else {
        return;
    };
    let removed = match c.source {
        CandidateSource::Favorite => {
            let mut favs = state.settings.filejump_favorites.clone();
            favs.retain(|f| f.path().to_lowercase() != c.path.to_lowercase());
            crate::filejump::persist_lists(
                &deps.settings_path,
                &favs,
                &state.settings.filejump_recent,
                state.settings.filejump_recent_max,
            );
            state.fj.hint = "已从收藏移除".to_string();
            true
        }
        CandidateSource::Recent => {
            let mut recent = state.settings.filejump_recent.clone();
            recent.retain(|f| f.to_lowercase() != c.path.to_lowercase());
            crate::filejump::persist_lists(
                &deps.settings_path,
                &state.settings.filejump_favorites,
                &recent,
                state.settings.filejump_recent_max,
            );
            state.fj.hint = "已从最近移除".to_string();
            true
        }
        _ => false,
    };
    if removed {
        state.fj.items.remove(idx);
        state.fj.filtered = crate::filejump::filter_items(&state.fj.items, &state.fj.query);
        state.fj.selected = state
            .fj
            .selected
            .min(state.fj.filtered.len().saturating_sub(1));
        crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
    }
}

fn fj_scroll_page(state: &mut State, deps: &LogicDeps, direction: i32) {
    let n = state.fj.filtered.len();
    if n == 0 {
        return;
    }
    let vis = n.min(8).max(1);
    let rel = state.fj.selected.saturating_sub(state.fj.first_visible);
    let max_first = n.saturating_sub(vis);
    let new_first = (state.fj.first_visible as i32 + direction * 8).clamp(0, max_first as i32) as usize;
    state.fj.first_visible = new_first;
    state.fj.selected = (new_first + rel).min(n - 1);
    crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
}

fn fj_ensure_visible(fj: &mut crate::filejump::FjState) {
    let n = fj.filtered.len();
    if n == 0 {
        fj.first_visible = 0;
        return;
    }
    let vis = n.min(8).max(1);
    if fj.selected < fj.first_visible {
        fj.first_visible = fj.selected;
    } else if fj.selected >= fj.first_visible + vis {
        fj.first_visible = fj.selected + 1 - vis;
    }
    let max_first = n.saturating_sub(vis);
    if fj.first_visible > max_first {
        fj.first_visible = max_first;
    }
}

fn fj_key(k: KeyEvt, state: &mut State, deps: &LogicDeps) {
    let n = state.fj.filtered.len();
    match k {
        KeyEvt::Esc => {
            if !state.fj.query.is_empty() {
                state.fj.query.clear();
                fj_refilter(state, deps, false);
            } else {
                fj_hide(state, deps);
            }
        }
        KeyEvt::Enter => fj_activate(state, deps),
        KeyEvt::Up => {
            if n > 0 {
                state.fj.selected = state.fj.selected.saturating_sub(1);
                fj_ensure_visible(&mut state.fj);
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            }
        }
        KeyEvt::Down => {
            if n > 0 {
                state.fj.selected = (state.fj.selected + 1).min(n - 1);
                fj_ensure_visible(&mut state.fj);
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            }
        }
        KeyEvt::PgUp | KeyEvt::Left => {
            if n > 0 {
                fj_scroll_page(state, deps, -1);
            }
        }
        KeyEvt::PgDn | KeyEvt::Right => {
            if n > 0 {
                fj_scroll_page(state, deps, 1);
            }
        }
        KeyEvt::QuickNum(d) => {
            if let Some(sel) = index_of_display(d, state.fj.first_visible, n) {
                state.fj.selected = sel;
                fj_activate(state, deps);
            }
        }
        KeyEvt::Home => {
            state.fj.selected = 0;
            state.fj.first_visible = 0;
            crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
        }
        KeyEvt::End => {
            if n > 0 {
                state.fj.selected = n - 1;
                fj_ensure_visible(&mut state.fj);
                crate::filejump::push_fj_ui(&deps.fj, &state.fj, false, state.fj.dialog);
            }
        }
        KeyEvt::Backspace => {
            if state.fj.query.pop().is_some() {
                fj_refilter(state, deps, true);
            }
        }
        KeyEvt::Delete => fj_remove_selected(state, deps),
        KeyEvt::Menu | KeyEvt::PinToggle => fj_toggle_favorite(state, deps),
        KeyEvt::QuickTab => {
            state.fj.fav_only = !state.fj.fav_only;
            fj_refilter(state, deps, false);
        }
        KeyEvt::Char(c) => {
            if state.fj.query.chars().count() < 64 {
                state.fj.query.push(c);
                fj_refilter(state, deps, true);
            }
        }
        KeyEvt::Digit(d) => {
            if state.fj.query.chars().count() < 64 {
                state.fj.query.push((b'0' + d) as char);
                fj_refilter(state, deps, true);
            }
        }
        KeyEvt::Space => {
            if state.fj.query.chars().count() < 64 {
                state.fj.query.push(' ');
                fj_refilter(state, deps, true);
            }
        }
        _ => {}
    }
}

// ================= 设置应用（Phase A）=================

/// 保存成功后生效：写盘已在 handle_save 前完成（settings::save），
/// 此处更新真相源 + 全部副作用（热键重注册/自启/钩子开关/策略/watcher/托盘）。
fn apply_settings(state: &mut State, deps: &LogicDeps, weak: &slint::Weak<PopupWindow>) -> bool {
    let draft = state.settings_win.draft_settings().clone();
    if crate::settings::save(&deps.settings_path, &draft).is_err() {
        state.settings_win.set_error("写设置文件失败".to_string());
        return false;
    }
    state.settings = draft;
    let s = &state.settings;
    deps.store.set_limits(clipx_store::StoreLimits {
        max_items: s.max_items,
        max_image_items: s.max_image_items,
    });
    // 热键重注册（含 Win+V 替换）。
    let _ = deps.hotkey_tx.send(crate::hotkeys_from_settings(s));
    // 开机自启（含管理员）。
    crate::autostart::set(s.run_at_startup, s.run_as_admin);
    // 钩子与策略。
    crate::keyboard_hook::set_qf_enabled(s.explorer_everything_quickfind_enabled);
    crate::keyboard_hook::set_page_hotkeys(s.page_up, s.page_down);
    crate::keyboard_hook::set_app_hotkeys(s.hotkey, s.batch_hotkey, s.filejump_hotkey);
    crate::keyboard_hook::set_passthrough(
        s.passthrough_enabled,
        s.passthrough_mask,
        s.passthrough_keep_panel_keys,
        &s.passthrough_rules,
    );
    crate::filejump::set_follow_mouse(s.filejump_follow_mode == "Mouse");
    crate::policy::set_exclusions(&s.exclusion_apps);
    crate::policy::set_panel_key(&s.panel_key);
    crate::policy::set_ocr_enabled(s.image_ocr_enabled);
    crate::policy::apply_win_v_replace(s.replace_win_v);
    crate::keyboard_hook::set_replace_win_v(s.replace_win_v);
    clipx_monitor::set_max_image_bytes(s.max_image_bytes);
    clipx_filejump::custom::set_runtime_rules(
        clipx_filejump::custom::CustomStore::load(&custom_dialogs_path(&deps.settings_path)).rules,
    );
    crate::filejump::watch_set(
        s.filejump_enabled,
        s.filejump_auto_popup,
        s.filejump_show_delay_ms,
    );
    crate::settings_win::apply_theme(&s.theme, weak);
    sync_batch_watch(state);
    refresh_tray(state, deps);
    let want = s.run_as_admin;
    let have = crate::autostart::is_elevated();
    if crate::autostart::should_auto_elevate() {
        if want && !have {
            if crate::autostart::restart_elevated() {
                request_quit();
            }
        } else if !want && have && crate::autostart::restart_unelevated() {
            request_quit();
        }
    }
    true
}

fn request_quit() {
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

fn sync_edit_chrome(state: &State, weak: &slint::Weak<PopupWindow>) {
    let mode = if state.text_edit.is_some() {
        1
    } else if state.phrase_edit.is_some() {
        2
    } else {
        0
    };
    crate::keyboard_hook::set_edit_mode(mode);
    let on = mode != 0;
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            crate::win_popup::set_edit_activate(&ui.window(), on);
        }
    });
}

fn notify(state: &mut State, deps: &LogicDeps, msg: impl Into<String>) {
    let msg = msg.into();
    state.notice = msg.clone();
    state.settings_win.set_notice(msg);
    if state.settings_win.open {
        crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
    }
    refresh_tray(state, deps);
}

fn refresh_tray(state: &State, deps: &LogicDeps) {
    let Some(tray) = deps.tray.as_ref() else {
        return;
    };
    let paused = crate::policy::is_capture_paused();
    let autostart_label: slint::SharedString = if crate::autostart::is_enabled() {
        "开机自启：开".into()
    } else {
        "开机自启：关".into()
    };
    let pause_label: slint::SharedString = if paused {
        "继续采集".into()
    } else {
        "暂停采集".into()
    };
    let armed = state
        .clear_armed_at
        .map(|t| t.elapsed() < Duration::from_secs(5))
        .unwrap_or(false);
    let clear_label: slint::SharedString = if armed {
        "再点一次确认清空".into()
    } else {
        "清空历史".into()
    };
    let tip: slint::SharedString = {
        let batch = match state.settings.batch_mode.as_str() {
            "Fifo" => " · FIFO",
            "Lifo" => " · LIFO",
            _ => "",
        };
        if !state.notice.is_empty() {
            format!("clipx{batch} · {}", state.notice).into()
        } else if paused {
            format!("clipx · 已暂停采集{batch} ({})", state.settings.hotkey.display()).into()
        } else if let Some(tag) = state.settings.last_update_tag.as_ref() {
            format!("clipx{batch} · 新版本 {tag} ({})", state.settings.hotkey.display()).into()
        } else {
            format!("clipx{batch} ({})", state.settings.hotkey.display()).into()
        }
    };
    let batch_mode = state.settings.batch_mode.clone();
    let tray = tray.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(t) = tray.upgrade() {
            t.set_autostart_label(autostart_label);
            t.set_pause_label(pause_label);
            t.set_clear_label(clear_label);
            t.set_tip_text(tip);
            t.set_glyph(tray_glyph_for_mode(&batch_mode));
        }
    });
}

fn tray_glyph_for_mode(mode: &str) -> slint::Image {
    let bytes = include_bytes!("../assets/tray.png");
    let img = image::load_from_memory(bytes).unwrap_or_else(|_| image::DynamicImage::new_rgba8(16, 16));
    let mut rgba = img.to_rgba8();
    // 与 WPF TrayIconSvg 同档配色：主体主色 + 中间横条浅色，按亮度分档保留层次。
    let (main, bar) = match mode {
        "Fifo" => ((37u8, 99, 235), (191u8, 219, 254)),
        "Lifo" => ((202u8, 138, 4), (254u8, 240, 138)),
        _ => ((19u8, 148, 147), (181u8, 232, 231)),
    };
    for p in rgba.pixels_mut() {
        if p[3] == 0 {
            continue;
        }
        let lum = (p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000;
        let (r, g, b) = if lum > 170 { bar } else { main };
        p[0] = r;
        p[1] = g;
        p[2] = b;
    }
    let (w, h) = rgba.dimensions();
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
    slint::Image::from_rgba8(buf)
}

/// 清空历史两段确认（WPF 二次确认语义；快捷短语存 JSON 不受影响）。
fn settings_clear_flow(state: &mut State, deps: &LogicDeps) {
    if !state.settings_win.clear_armed() {
        state.settings_win.arm_clear();
        crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
        return;
    }
    let n = deps.store.clear_all();
    state.settings_win.disarm_clear();
    state.settings_win.set_error(format!("已清空 {n} 条历史（快捷短语保留）"));
    // 主列表同步刷新。
    state.thumb_cache.clear();
    state.thumb_inflight.clear();
    state.preview_cache.clear();
    state.file_preview_cache.clear();
    let _ = std::fs::remove_dir_all(&deps.rendition_dir);
    state.batch_queue.clear();
    sync_batch_watch(state);
    state.query.clear();
    state.items = merge_list_items(state, deps);
    select_single(state, 0);
    crate::settings_win::push(&deps.settings_win.borrow(), &state.settings_win);
}

fn settings_excl_add(state: &mut State) {
    if !state.settings_win.excl_add_current() {
        state
            .settings_win
            .set_error("已在名单中或未选择进程".to_string());
    } else {
        state.settings_win.set_error(String::new());
    }
}

fn settings_excl_del(state: &mut State, i: i32) {
    state.settings_win.excl_remove(i.max(0) as usize);
}

fn settings_custom_del(state: &mut State, i: i32) {
    if !state.settings_win.custom_remove(i.max(0) as usize) {
        state.settings_win.set_error("删除失败".to_string());
    } else {
        state.settings_win.set_error(String::new());
    }
}

fn settings_custom_import(state: &mut State) {
    let msg = state.settings_win.custom_import();
    state.settings_win.set_error(msg);
}

fn settings_custom_export(state: &mut State) {
    let msg = state.settings_win.custom_export();
    state.settings_win.set_error(msg);
}

fn tray_clear_flow(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    let armed = state
        .clear_armed_at
        .map(|t| t.elapsed() < Duration::from_secs(5))
        .unwrap_or(false);
    if !armed {
        state.clear_armed_at = Some(std::time::Instant::now());
        refresh_tray(state, deps);
        return;
    }
    state.clear_armed_at = None;
    let n = deps.store.clear_all();
    state.thumb_cache.clear();
    state.thumb_inflight.clear();
    state.preview_cache.clear();
    state.file_preview_cache.clear();
    let _ = std::fs::remove_dir_all(&deps.rendition_dir);
    state.batch_queue.clear();
    sync_batch_watch(state);
    state.query.clear();
    state.items = merge_list_items(state, deps);
    select_single(state, 0);
    if state.visible {
        push_ui(state, weak);
    }
    refresh_tray(state, deps);
    let _ = n;
}

/// 粘贴触顶（WPF `TouchCopiedTime`）。设置关则保持原序；快捷短语不在库中。
fn touch_pasted(state: &State, deps: &LogicDeps, id: i64) {
    if !state.settings.paste_touch_top || is_phrase_id(id) {
        return;
    }
    deps.store.touch(id);
}

fn is_phrase_id(id: i64) -> bool {
    id < 0
}

fn phrase_entry_id(index: usize) -> i64 {
    -(index as i64 + 1)
}

fn phrase_index_of(id: i64) -> Option<usize> {
    if id < 0 {
        Some(((-id) as usize).saturating_sub(1))
    } else {
        None
    }
}

fn phrase_matches(qp: &QuickPaste, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let ql = q.to_lowercase();
    if qp.phrase.to_lowercase().contains(&ql) || qp.content.to_lowercase().contains(&ql) {
        return true;
    }
    let blob = to_pinyin_blob(&format!("{}{}", qp.phrase, qp.content));
    !blob.is_empty() && blob.contains(&ql)
}

fn truncate_preview(text: &str, max_lines: i64) -> String {
    let max_lines = max_lines.clamp(1, 10) as usize;
    const MAX_CHARS: usize = 200;
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let taken: Vec<&str> = lines.iter().take(max_lines).copied().collect();
    let mut result = taken.join("\n");
    if result.chars().count() > MAX_CHARS {
        let end = result
            .char_indices()
            .nth(MAX_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(result.len());
        result.truncate(end);
        result.push('…');
    } else if lines.len() > max_lines {
        result.push_str(" …");
    }
    result
}

fn phrase_metas(settings: &Settings, query: &str) -> Vec<EntryMeta> {
    settings
        .phrases
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.content.is_empty() && phrase_matches(p, query))
        .map(|(i, p)| EntryMeta {
            id: phrase_entry_id(i),
            kind: EntryKind::Text,
            preview: truncate_preview(&p.content, settings.preview_max_lines),
            pinned: false,
            created_ms: 0,
            source_app: String::new(),
        })
        .collect()
}

fn merge_list_items(state: &State, deps: &LogicDeps) -> Vec<EntryMeta> {
    let phrases = if state.filter.is_none()
        || matches!(state.filter, Some(EntryKind::Text | EntryKind::RichText))
        || state.phrase_only
    {
        phrase_metas(&state.settings, &state.query)
    } else {
        Vec::new()
    };
    if state.phrase_only {
        return phrases;
    }
    let hist = deps.store.search_ex(
        &state.query,
        state.filter,
        state.source_filter.as_deref(),
        state.settings.deep_search,
        list_limit(&state.settings),
    );
    if state.query.trim().is_empty() {
        let mut out = hist;
        out.extend(phrases);
        reorder_queue_first(state, &mut out);
        out
    } else {
        let mut out = phrases;
        out.extend(hist);
        rank_results(&mut out, &state.query);
        out
    }
}

/// 对齐 WPF `ReorderAllItemsQueueFirst` + `UpdateBatchOrderProperties`：
/// 无搜索/筛选时队列条目置顶（按队列顺序），行内角标见 build_rows 的「队列 n」。
fn reorder_queue_first(state: &State, out: &mut Vec<EntryMeta>) {
    if state.settings.batch_mode == "Off" || state.batch_queue.is_empty() {
        return;
    }
    if !state.query.trim().is_empty()
        || state.filter.is_some()
        || state.source_filter.is_some()
        || state.phrase_only
    {
        return;
    }
    let mut queued = Vec::with_capacity(state.batch_queue.len());
    let mut rest = Vec::with_capacity(out.len());
    for m in out.drain(..) {
        if state.batch_queue.contains(&m.id) {
            queued.push(m);
        } else {
            rest.push(m);
        }
    }
    queued.sort_by_key(|m| {
        state
            .batch_queue
            .iter()
            .position(|id| *id == m.id)
            .unwrap_or(usize::MAX)
    });
    queued.extend(rest);
    *out = queued;
}

fn delete_item(state: &mut State, deps: &LogicDeps, id: i64) {
    state.preview_cache.remove(&id);
    crate::preview_rendition::remove(&deps.rendition_dir, id);
    if let Some(idx) = phrase_index_of(id) {
        if idx < state.settings.phrases.len() {
            state.settings.phrases.remove(idx);
            let _ = crate::settings::save(&deps.settings_path, &state.settings);
        }
    } else {
        deps.store.delete(id);
    }
}

fn begin_phrase_edit(
    state: &mut State,
    deps: &LogicDeps,
    meta: &EntryMeta,
    weak: &slint::Weak<PopupWindow>,
) {
    if let Some(idx) = phrase_index_of(meta.id) {
        if let Some(p) = state.settings.phrases.get(idx).cloned() {
            state.phrase_edit = Some(PhraseEdit {
                content: p.content,
                buffer: p.phrase,
            });
        }
        sync_edit_chrome(state, weak);
        return;
    }
    if !matches!(meta.kind, EntryKind::Text | EntryKind::RichText) {
        return;
    }
    let content = deps.store.get_text(meta.id).unwrap_or_default();
    if content.is_empty() {
        return;
    }
    state.phrase_edit = Some(PhraseEdit {
        content,
        buffer: String::new(),
    });
    sync_edit_chrome(state, weak);
}

fn handle_phrase_edit_key(
    k: KeyEvt,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    match k {
        KeyEvt::Esc => {
            state.phrase_edit = None;
            sync_edit_chrome(state, weak);
            push_ui(state, weak);
        }
        KeyEvt::Enter => commit_phrase_edit(state, deps, weak),
        _ => {}
    }
}

fn commit_phrase_edit(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    let Some(edit) = state.phrase_edit.take() else {
        push_ui(state, weak);
        return;
    };
    let phrase = edit.buffer.trim().to_string();
    if phrase.is_empty() {
        push_ui(state, weak);
        return;
    }
    state
        .settings
        .phrases
        .retain(|p| p.content != edit.content);
    state.settings.phrases.push(QuickPaste {
        phrase,
        content: edit.content,
    });
    let _ = crate::settings::save(&deps.settings_path, &state.settings);
    sync_edit_chrome(state, weak);
    refresh(state, deps, weak, false);
}

fn row_picked(_current: usize, sel_set: &BTreeSet<usize>, i: usize) -> bool {
    // 单选不高亮靠 picked：方向键只更新 selected-index，baked picked 会把首行钉死。
    sel_set.len() > 1 && sel_set.contains(&i)
}

fn selected_indices(state: &State) -> Vec<usize> {
    let n = state.items.len();
    if n == 0 {
        return Vec::new();
    }
    if state.sel_set.len() > 1 {
        state
            .sel_set
            .iter()
            .copied()
            .filter(|&i| i < n)
            .collect()
    } else {
        vec![state.selected.min(n - 1)]
    }
}

fn selected_count(state: &State) -> usize {
    selected_indices(state).len()
}

fn fill_inclusive(set: &mut BTreeSet<usize>, a: usize, b: usize) {
    set.clear();
    let (lo, hi) = (a.min(b), a.max(b));
    for i in lo..=hi {
        set.insert(i);
    }
}

/// 单击 / 右键 / 中键 / 方向键：只留当前行。
fn select_single(state: &mut State, idx: usize) {
    if state.items.is_empty() {
        state.selected = 0;
        state.sel_anchor = 0;
        state.sel_set.clear();
        return;
    }
    let idx = idx.min(state.items.len() - 1);
    state.selected = idx;
    state.sel_anchor = idx;
    state.sel_set.clear();
    state.sel_set.insert(idx);
}

/// Shift+↑↓ / Shift+单击：锚点到当前行的闭区间。
fn select_extend(state: &mut State, idx: usize) {
    if state.items.is_empty() {
        return;
    }
    let idx = idx.min(state.items.len() - 1);
    state.selected = idx;
    fill_inclusive(&mut state.sel_set, state.sel_anchor, idx);
}

/// Ctrl+单击：加减条目（至少保留一行）。
fn select_toggle(state: &mut State, idx: usize) {
    if state.items.is_empty() {
        return;
    }
    let idx = idx.min(state.items.len() - 1);
    if state.sel_set.is_empty() {
        state.sel_set.insert(state.selected.min(state.items.len() - 1));
    }
    if state.sel_set.contains(&idx) {
        if state.sel_set.len() > 1 {
            state.sel_set.remove(&idx);
        }
    } else {
        state.sel_set.insert(idx);
    }
    state.selected = if state.sel_set.contains(&idx) {
        idx
    } else {
        *state.sel_set.iter().next().unwrap_or(&0)
    };
    state.sel_anchor = idx;
}

fn clamp_selection(state: &mut State) {
    let n = state.items.len();
    if n == 0 {
        state.selected = 0;
        state.sel_anchor = 0;
        state.sel_set.clear();
        return;
    }
    state.selected = state.selected.min(n - 1);
    state.sel_anchor = state.sel_anchor.min(n - 1);
    state.sel_set.retain(|&i| i < n);
    if state.sel_set.is_empty() {
        state.sel_set.insert(state.selected);
    } else if !state.sel_set.contains(&state.selected) {
        state.selected = *state.sel_set.iter().next().unwrap_or(&0);
    }
}

/// `ql` 为调用方预计算的小写查询（每刷新一次只算一次，不要每行重复算）。
/// 用 `get` 安全切片：小写化可能改变字节长度，直接按下标切会 panic。
fn split_hit(preview: &str, ql: &str) -> (String, String, String) {
    if ql.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    let pl = preview.to_lowercase();
    if let Some(byte) = pl.find(ql) {
        let end = byte + ql.len();
        if let (Some(pre), Some(hit), Some(post)) =
            (preview.get(..byte), preview.get(byte..end), preview.get(end..))
        {
            return (pre.to_string(), hit.to_string(), post.to_string());
        }
    }
    (String::new(), String::new(), String::new())
}

fn rank_results(items: &mut [EntryMeta], query: &str) {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return;
    }
    // Schwartzian：每行的小写预览与分只算一次。原来 sort_by 的每次比较都调
    // 两次 to_lowercase，2000 行约 2 万次全行小写化，是有查询时组行的主开销。
    let now = now_ms();
    let mut lowered = Vec::with_capacity(items.len());
    let mut scores = Vec::with_capacity(items.len());
    for m in items.iter() {
        let pl = m.preview.to_lowercase();
        scores.push(rank_score(m, &pl, &q, now));
        lowered.push(pl);
    }
    let mut idx: Vec<usize> = (0..items.len()).collect();
    idx.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut tmp = Vec::with_capacity(items.len());
    for i in idx {
        tmp.push(items[i].clone());
    }
    items.clone_from_slice(&tmp);
}

fn rank_score(m: &EntryMeta, preview_lower: &str, q: &str, now: i64) -> f64 {
    let mut s = 0.0;
    if is_phrase_id(m.id) {
        s += 80.0;
    }
    if m.pinned {
        s += 40.0;
    }
    let p = preview_lower;
    if *p == *q {
        s += 50.0;
    } else if p.starts_with(q) {
        s += 25.0;
    } else if p.contains(q) {
        s += 10.0;
    }
    let age_h = ((now - m.created_ms).max(0) as f64) / 3_600_000.0;
    s += (-age_h / 72.0).exp() * 8.0;
    s
}

fn paste_selection(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    with_newlines: bool,
) {
    if state.settings.batch_mode != "Off" {
        activate_item(state, deps, weak, clipboard, state.selected);
        return;
    }
    let idxs = selected_indices(state);
    if idxs.len() <= 1 {
        activate_item(
            state,
            deps,
            weak,
            clipboard,
            idxs.first().copied().unwrap_or(state.selected),
        );
        return;
    }
    let ids: Vec<(i64, EntryKind)> = idxs
        .iter()
        .filter_map(|&i| state.items.get(i).map(|m| (m.id, m.kind)))
        .collect();
    paste_ordered(state, deps, weak, clipboard, &ids, with_newlines);
}

fn is_text_seg(id: i64, kind: EntryKind) -> bool {
    is_phrase_id(id) || matches!(kind, EntryKind::Text | EntryKind::RichText)
}

#[derive(Debug, Clone, PartialEq)]
enum PasteSeg {
    Single(i64, EntryKind),
    MergeText(Vec<(i64, EntryKind)>),
    MergeFiles(Vec<(i64, EntryKind)>),
}

/// 对齐 WPF `BuildAdjacentRuns`：文本 vs 非文本分段；段内 ≥2 条再合并。
fn adjacent_paste_segs(ids: &[(i64, EntryKind)]) -> Vec<PasteSeg> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let anchor_text = is_text_seg(ids[i].0, ids[i].1);
        let mut run = vec![ids[i]];
        i += 1;
        while i < ids.len() && is_text_seg(ids[i].0, ids[i].1) == anchor_text {
            run.push(ids[i]);
            i += 1;
        }
        if run.len() >= 2 && anchor_text {
            out.push(PasteSeg::MergeText(run));
        } else if run.len() >= 2 {
            out.push(PasteSeg::MergeFiles(run));
        } else {
            for (id, kind) in run {
                out.push(PasteSeg::Single(id, kind));
            }
        }
    }
    out
}

fn paste_nl() -> &'static str {
    if cfg!(windows) {
        "\r\n"
    } else {
        "\n"
    }
}

fn paste_ordered(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    ids: &[(i64, EntryKind)],
    with_newlines: bool,
) -> bool {
    if ids.is_empty() {
        return false;
    }
    let merge = state.settings.batch_merge_text;
    let for_console = {
        #[cfg(windows)]
        {
            paste::is_console_target(state.foreground_at_show)
        }
        #[cfg(not(windows))]
        {
            false
        }
    };
    if merge && ids.iter().all(|(id, k)| is_text_seg(*id, *k)) {
        let Some(merged) = write_merged_clipboard(
            ids,
            deps,
            clipboard,
            &state.settings,
            with_newlines,
            for_console,
        ) else {
            return false;
        };
        apply_merged_write(state, deps, merged, ids);
        finish_paste(state, weak);
        if state.window_pinned {
            refresh(state, deps, weak, true);
        }
        return true;
    }
    let segs = if merge {
        adjacent_paste_segs(ids)
    } else {
        ids.iter()
            .copied()
            .map(|(id, k)| PasteSeg::Single(id, k))
            .collect()
    };
    run_paste_segments(state, deps, weak, clipboard, &segs, ids, with_newlines)
}

fn run_paste_segments(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    segs: &[PasteSeg],
    orig: &[(i64, EntryKind)],
    with_newlines: bool,
) -> bool {
    if segs.is_empty() {
        return false;
    }
    let target = {
        #[cfg(windows)]
        {
            state.foreground_at_show
        }
        #[cfg(not(windows))]
        {
            0
        }
    };
    let pinned = state.window_pinned;
    if !pinned {
        hide_popup(state, weak);
    }
    let own = crate::mouse_hook::POPUP_HWND.load(std::sync::atomic::Ordering::SeqCst);
    let mut any_merged = false;
    let mut ok_any = false;
    for (i, seg) in segs.iter().enumerate() {
        let after_image = match seg {
            PasteSeg::Single(_, kind) => matches!(kind, EntryKind::Image | EntryKind::Files),
            PasteSeg::MergeFiles(_) => true,
            PasteSeg::MergeText(_) => false,
        };
        let ok = match seg {
            PasteSeg::Single(id, kind) => paste_one_id(
                state,
                deps,
                clipboard,
                *id,
                *kind,
                with_newlines && is_text_seg(*id, *kind),
                i == 0,
                target,
            ),
            PasteSeg::MergeText(group) => {
                any_merged = true;
                if let Some(m) = write_merged_clipboard(
                    group,
                    deps,
                    clipboard,
                    &state.settings,
                    with_newlines,
                    paste::is_console_target(target),
                ) {
                    apply_merged_write(state, deps, m, group);
                    send_segment_paste(state, target, i == 0);
                    true
                } else {
                    false
                }
            }
            PasteSeg::MergeFiles(group) => {
                any_merged = true;
                if let Some(m) = write_merged_clipboard(
                    group,
                    deps,
                    clipboard,
                    &state.settings,
                    false,
                    paste::is_console_target(target),
                ) {
                    apply_merged_write(state, deps, m, group);
                    send_segment_paste(state, target, i == 0);
                    true
                } else {
                    false
                }
            }
        };
        ok_any |= ok;
        if i + 1 < segs.len() {
            paste::wait_clipboard_consumed(own, after_image);
            std::thread::sleep(Duration::from_millis(22));
        }
    }
    if !any_merged {
        for (id, _) in orig.iter().rev() {
            touch_pasted(state, deps, *id);
        }
    }
    state.last_paste_at = Some(std::time::Instant::now());
    if pinned {
        refresh(state, deps, weak, true);
    }
    std::thread::sleep(Duration::from_millis(85));
    ok_any
}

fn send_segment_paste(state: &State, target: isize, first: bool) {
    if !state.settings.paste_simulate {
        return;
    }
    if first {
        std::thread::sleep(Duration::from_millis(80));
        #[cfg(windows)]
        win_popup::restore_foreground(target);
        std::thread::sleep(Duration::from_millis(30));
    } else {
        #[cfg(windows)]
        win_popup::restore_foreground(target);
        std::thread::sleep(Duration::from_millis(15));
    }
    paste::send_paste(paste::paste_mode_for_target(
        target,
        &state.settings.paste_mode,
    ));
}

fn paste_one_id(
    state: &State,
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
    id: i64,
    kind: EntryKind,
    append_nl: bool,
    first: bool,
    target: isize,
) -> bool {
    let Some(meta) = meta_by_id(state, deps, id) else {
        return false;
    };
    let for_console = paste::is_console_target(target);
    let wrote = if append_nl {
        write_entry_text_nl(&meta, deps, clipboard, &state.settings, for_console)
    } else {
        write_entry_clipboard(&meta, deps, clipboard, &state.settings, for_console)
    };
    if !wrote {
        return false;
    }
    let _ = kind;
    send_segment_paste(state, target, first);
    true
}

fn write_entry_text_nl(
    meta: &EntryMeta,
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
    settings: &Settings,
    for_console: bool,
) -> bool {
    let Some(ctx) = clipboard else {
        return false;
    };
    let mut text = if let Some(idx) = phrase_index_of(meta.id) {
        settings
            .phrases
            .get(idx)
            .map(|p| p.content.clone())
            .unwrap_or_default()
    } else {
        deps.store.get_text(meta.id).unwrap_or_default()
    };
    if text.is_empty() {
        return false;
    }
    if for_console {
        text = paste::normalize_console_text(&text);
        text.push('\n');
    } else {
        text.push_str(paste_nl());
    }
    deps.gate.arm();
    paste::write_text(ctx, &text).is_ok()
}

fn apply_merged_write(
    state: &State,
    deps: &LogicDeps,
    merged: MergedWrite,
    ids: &[(i64, EntryKind)],
) {
    match merged {
        MergedWrite::Text(text) => {
            // 对齐 WPF InsertBatchMergedEntry：合并产物入库（门已 arm，不会自采）。
            if let Err(e) = deps.store.insert(NewEntry::from_text(text)) {
                eprintln!("合并粘贴入库失败: {e}");
            }
        }
        MergedWrite::Files(paths) => {
            if let Err(e) = deps.store.insert(NewEntry::from_files(paths)) {
                eprintln!("合并文件入库失败: {e}");
            }
        }
        MergedWrite::Plain => {
            for (id, _) in ids.iter().rev() {
                touch_pasted(state, deps, *id);
            }
        }
    }
}

enum MergedWrite {
    /// 多段文本拼成一段（WPF 合并粘贴产物）。
    Text(String),
    /// 多文件/图落成一组 FileDrop。
    Files(Vec<String>),
    /// 写了剪贴板但没有新历史行（关合并时的一次写出）。
    Plain,
}

fn write_merged_clipboard(
    ids: &[(i64, EntryKind)],
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
    settings: &Settings,
    with_newlines: bool,
    for_console: bool,
) -> Option<MergedWrite> {
    let ctx = clipboard?;
    let mut texts = Vec::new();
    let mut images = Vec::new();
    let mut files = Vec::new();
    for &(id, kind) in ids {
        if is_phrase_id(id) {
            if let Some(idx) = phrase_index_of(id) {
                if let Some(p) = settings.phrases.get(idx) {
                    texts.push(p.content.clone());
                }
            }
            continue;
        }
        match kind {
            EntryKind::Text | EntryKind::RichText => {
                if let Some(t) = deps.store.get_text(id) {
                    texts.push(t);
                }
            }
            EntryKind::Image => {
                if let Some(img) = deps.store.get_image(id) {
                    images.push(img);
                }
            }
            EntryKind::Files => {
                if let Some(p) = deps.store.get_files(id) {
                    files.extend(p);
                }
            }
        }
    }
    deps.gate.arm();
    if !files.is_empty() || !images.is_empty() {
        let mut drop_files = files;
        let dir = std::env::temp_dir().join("clipx-paste");
        if std::fs::create_dir_all(&dir).is_ok() {
            for (i, img) in images.iter().enumerate() {
                let p = dir.join(format!("clipx-img-{i}.png"));
                if std::fs::write(&p, &img.blob).is_ok() {
                    drop_files.push(p.to_string_lossy().into_owned());
                }
            }
        }
        if paste::write_files(ctx, &drop_files).is_ok() {
            return Some(if drop_files.len() >= 2 {
                MergedWrite::Files(drop_files)
            } else {
                MergedWrite::Plain
            });
        }
        return None;
    }
    if texts.is_empty() {
        return None;
    }
    let joined = if with_newlines {
        let sep = if for_console { "\n" } else { paste_nl() };
        texts.join(sep)
    } else {
        texts.concat()
    };
    if paste::write_text_for_target(ctx, &joined, for_console).is_ok() {
        return Some(if texts.len() >= 2 {
            MergedWrite::Text(joined)
        } else {
            MergedWrite::Plain
        });
    }
    None
}

fn finish_paste(state: &mut State, weak: &slint::Weak<PopupWindow>) {
    state.last_paste_at = Some(std::time::Instant::now());
    let target = {
        #[cfg(windows)]
        {
            state.foreground_at_show
        }
        #[cfg(not(windows))]
        {
            0
        }
    };
    if !state.window_pinned {
        hide_popup(state, weak);
    }
    if state.settings.paste_simulate {
        std::thread::sleep(Duration::from_millis(80));
        #[cfg(windows)]
        win_popup::restore_foreground(target);
        std::thread::sleep(Duration::from_millis(30));
        paste::send_paste(paste::paste_mode_for_target(
            target,
            &state.settings.paste_mode,
        ));
    }
}

fn paste_ocr(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
) {
    let Some(meta) = state.items.get(idx).cloned() else {
        return;
    };
    let Some(ctx) = clipboard else {
        return;
    };
    let text = if meta.kind == EntryKind::Image {
        deps.store
            .get_ocr(meta.id)
            .and_then(|o| o.text)
            .unwrap_or_default()
    } else {
        deps.store.get_text(meta.id).unwrap_or_default()
    };
    if text.trim().is_empty() {
        return;
    }
    deps.gate.arm();
    if paste::write_text(ctx, &text).is_err() {
        return;
    }
    finish_paste(state, weak);
}

fn paste_as_file(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
    idx: usize,
    as_json: bool,
) {
    let Some(meta) = state.items.get(idx).cloned() else {
        return;
    };
    let Some(ctx) = clipboard else {
        return;
    };
    let stamp = now_ms();
    let files = if as_json {
        let payload = serde_json::json!({
            "kind": meta.kind.as_i64(),
            "preview": meta.preview,
            "source_app": meta.source_app,
            "text": deps.store.get_text(meta.id),
            "files": deps.store.get_files(meta.id),
            "ocr": deps.store.get_ocr(meta.id).and_then(|o| o.text),
        });
        let bytes = serde_json::to_vec_pretty(&payload).unwrap_or_default();
        vec![(format!("clipx-{stamp}.json"), bytes)]
    } else {
        match meta.kind {
            EntryKind::Image => {
                let Some(img) = deps.store.get_image(meta.id) else {
                    return;
                };
                vec![(format!("clipx-{stamp}.png"), img.blob)]
            }
            EntryKind::Files => {
                if let Some(paths) = deps.store.get_files(meta.id) {
                    deps.gate.arm();
                    if paste::write_files(ctx, &paths).is_ok() {
                        finish_paste(state, weak);
                    }
                }
                return;
            }
            _ => {
                let text = deps.store.get_text(meta.id).unwrap_or(meta.preview.clone());
                vec![(format!("clipx-{stamp}.txt"), text.into_bytes())]
            }
        }
    };
    deps.gate.arm();
    if paste::write_temp_files(ctx, &files).is_ok() {
        finish_paste(state, weak);
    }
}

fn save_image_to_temp(
    state: &mut State,
    deps: &LogicDeps,
    meta: &EntryMeta,
    clipboard: Option<&ClipboardContext>,
) {
    if meta.kind != EntryKind::Image {
        return;
    }
    let Some(img) = deps.store.get_image(meta.id) else {
        return;
    };
    let dir = std::env::temp_dir().join("clipx-save");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("clipx-{}.png", now_ms()));
    if std::fs::write(&path, &img.blob).is_ok() {
        if let Some(ctx) = clipboard {
            deps.gate.arm();
            let _ = paste::write_text(ctx, &path.to_string_lossy());
        }
        notify(state, deps, format!("已保存：{}", path.display()));
    }
}

fn copy_entry_path(
    state: &mut State,
    deps: &LogicDeps,
    meta: &EntryMeta,
    clipboard: Option<&ClipboardContext>,
    weak: &slint::Weak<PopupWindow>,
) {
    let Some(ctx) = clipboard else {
        return;
    };
    let text = if meta.kind == EntryKind::Files {
        deps.store
            .get_files(meta.id)
            .map(|p| p.join("\n"))
            .unwrap_or_default()
    } else if meta.kind == EntryKind::Image {
        format!("clipx-image-{}", meta.id)
    } else {
        deps.store.get_text(meta.id).unwrap_or(meta.preview.clone())
    };
    if text.is_empty() {
        return;
    }
    deps.gate.arm();
    if paste::write_text(ctx, &text).is_ok() {
        hide_popup(state, weak);
    }
}

fn begin_text_edit(
    state: &mut State,
    deps: &LogicDeps,
    meta: &EntryMeta,
    weak: &slint::Weak<PopupWindow>,
) {
    if is_phrase_id(meta.id) || !matches!(meta.kind, EntryKind::Text | EntryKind::RichText) {
        push_ui(state, weak);
        return;
    }
    let text = deps.store.get_text(meta.id).unwrap_or_default();
    state.text_edit = Some(TextEdit {
        id: meta.id,
        buffer: text,
    });
    sync_edit_chrome(state, weak);
    push_ui(state, weak);
}

fn handle_text_edit_key(
    k: KeyEvt,
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    match k {
        KeyEvt::Esc => {
            state.text_edit = None;
            sync_edit_chrome(state, weak);
            push_ui(state, weak);
        }
        KeyEvt::CtrlEnter => commit_text_edit(state, deps, weak),
        _ => {}
    }
}

fn commit_text_edit(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    let Some(edit) = state.text_edit.take() else {
        return;
    };
    if edit.buffer.trim().is_empty() {
        state.text_edit = Some(edit);
        return;
    }
    let _ = deps.store.update_text(edit.id, edit.buffer);
    sync_edit_chrome(state, weak);
    refresh(state, deps, weak, false);
}

fn batch_flush_all(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
    clipboard: Option<&ClipboardContext>,
) {
    if state.batch_queue.is_empty() {
        return;
    }
    let ids: Vec<(i64, EntryKind)> = state
        .batch_queue
        .iter()
        .filter_map(|id| meta_by_id(state, deps, *id).map(|m| (m.id, m.kind)))
        .collect();
    if !paste_ordered(state, deps, weak, clipboard, &ids, false) {
        return;
    }
    state.batch_queue.clear();
    if state.settings.batch_auto_off_when_empty {
        state.settings.batch_mode = "Off".to_string();
        let _ = crate::settings::save(&deps.settings_path, &state.settings);
    }
    sync_batch_watch(state);
    refresh_tray(state, deps);
}

fn batch_enqueue_latest(
    state: &mut State,
    deps: &LogicDeps,
    clipboard: Option<&ClipboardContext>,
) {
    if state.settings.batch_mode == "Off" {
        return;
    }
    let Some(latest) = deps.store.list_recent(1).into_iter().next() else {
        return;
    };
    if state.batch_queue.contains(&latest.id) {
        return;
    }
    // FIFO 尾部追加队首不变时不写剪贴板（对齐 WPF「队首引用不变不推」，防互锁卡顿）。
    let head_before = state.batch_queue.first().copied();
    if state.settings.batch_mode == "Fifo" {
        state.batch_queue.push(latest.id);
    } else {
        state.batch_queue.insert(0, latest.id);
    }
    if state.batch_queue.first().copied() != head_before {
        if let Some(head) = state.batch_queue.first().copied() {
            if batch_head_still(state, head) {
                if let Some(meta) = meta_by_id(state, deps, head) {
                    let _ = write_entry_clipboard(&meta, deps, clipboard, &state.settings, false);
                }
            }
        }
    }
    sync_batch_watch(state);
}

fn tray_probe_dialog(state: &mut State, deps: &LogicDeps) {
    #[cfg(windows)]
    {
        let hwnd = crate::win_popup::foreground_hwnd();
        if hwnd == 0 {
            return;
        }
        let class = clipx_filejump::dialog::win::class_of_pub(hwnd).unwrap_or_default();
        let proc = crate::policy::foreground_app();
        let title = clipx_filejump::dialog::win::title_of_pub(hwnd).unwrap_or_default();
        let path = custom_dialogs_path(&deps.settings_path);
        let mut store = clipx_filejump::custom::CustomStore::load(&path);
        store.rules.push(clipx_filejump::custom::CustomRule {
            class: class.clone(),
            process: proc.clone(),
            title_contains: String::new(),
            strategy: "alt_d".into(),
        });
        let _ = store.save(&path);
        clipx_filejump::custom::set_runtime_rules(store.rules);
        notify(
            state,
            deps,
            format!("已添加规则：{class} + {proc} 「{title}」"),
        );
    }
    #[cfg(not(windows))]
    {
        let _ = (state, deps);
    }
}

fn custom_dialogs_path(settings_path: &std::path::Path) -> std::path::PathBuf {
    settings_path
        .parent()
        .unwrap_or(settings_path)
        .join("custom_file_dialogs.json")
}

fn export_history(state: &mut State, deps: &LogicDeps) {
    let path = deps
        .settings_path
        .parent()
        .unwrap_or(&deps.settings_path)
        .join("clipx-export.json");
    match deps.store.export_json(&path) {
        Ok(n) => {
            notify(state, deps, format!("已导出 {n} 条 → {}", path.display()));
        }
        Err(e) => {
            notify(state, deps, format!("导出失败：{e}"));
        }
    }
}

fn import_history(
    state: &mut State,
    deps: &LogicDeps,
    weak: &slint::Weak<PopupWindow>,
) {
    let path = deps
        .settings_path
        .parent()
        .unwrap_or(&deps.settings_path)
        .join("clipx-export.json");
    match deps.store.import_json(&path) {
        Ok(st) => {
            notify(
                state,
                deps,
                format!("导入完成：新增 {}，跳过 {}", st.inserted, st.skipped_dup),
            );
            if state.visible {
                refresh(state, deps, weak, true);
            }
        }
        Err(e) => {
            notify(state, deps, format!("导入失败：{e}"));
        }
    }
}

fn check_updates_now(state: &mut State, deps: &LogicDeps) {
    crate::update_check::spawn_delayed(
        deps.evt_tx.clone(),
        state.settings.last_update_tag.clone(),
        std::time::Duration::ZERO,
    );
    notify(state, deps, "正在检查更新…");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_window_covers_visible_with_overscan() {
        // 2000 行，首行 100，可视 18：上下各 16 预留。
        assert_eq!(row_window(100, 18, 2000), (84, 134));
    }

    #[test]
    fn row_window_clamps_head_and_tail() {
        assert_eq!(row_window(0, 18, 2000), (0, 34));
        // 尾部：end 顶满 len，base 正常前伸。
        assert_eq!(row_window(1990, 18, 2000), (1974, 2000));
        // 短列表：全包。
        assert_eq!(row_window(0, 18, 10), (0, 10));
        assert_eq!(row_window(0, 18, 0), (0, 0));
    }

    #[test]
    fn row_window_first_visible_beyond_end_clamps() {
        // 越界首行（滚轮估算抖动）：base 收到 len-1 以内，不 panic。
        let (b, e) = row_window(5000, 18, 2000);
        assert!(b < 2000 && e == 2000 && b < e);
    }

    #[test]
    fn phrase_id_roundtrip() {
        assert_eq!(phrase_entry_id(0), -1);
        assert_eq!(phrase_index_of(-1), Some(0));
        assert_eq!(phrase_index_of(-3), Some(2));
        assert_eq!(phrase_index_of(12), None);
        assert!(is_phrase_id(-1));
        assert!(!is_phrase_id(1));
    }

    #[test]
    fn phrase_matches_text_and_pinyin() {
        let qp = QuickPaste {
            phrase: "邮箱".into(),
            content: "hello@example.com".into(),
        };
        assert!(phrase_matches(&qp, ""));
        assert!(phrase_matches(&qp, "hello"));
        assert!(phrase_matches(&qp, "邮箱"));
        assert!(phrase_matches(&qp, "yx"));
        assert!(!phrase_matches(&qp, "zzzz"));
    }

    #[test]
    fn truncate_preview_respects_lines() {
        let t = "one\ntwo\nthree";
        assert_eq!(truncate_preview(t, 1), "one …");
        assert_eq!(truncate_preview(t, 2), "one\ntwo …");
        assert_eq!(truncate_preview(t, 3), "one\ntwo\nthree");
    }

    #[test]
    fn split_hit_keeps_surrounding_text() {
        assert_eq!(
            split_hit("time1", "ti"),
            (String::new(), "ti".into(), "me1".into())
        );
        assert_eq!(
            split_hit("timeout 5 bash", "ti"),
            (String::new(), "ti".into(), "meout 5 bash".into())
        );
        // 调用方传入已小写查询；大小写不敏感由调用方保证。
        assert_eq!(
            split_hit("notice title", "ti"),
            ("no".into(), "ti".into(), "ce title".into())
        );
        assert_eq!(
            split_hit("你好世界", "ti"),
            (String::new(), String::new(), String::new())
        );
        assert_eq!(
            split_hit("hello", ""),
            (String::new(), String::new(), String::new())
        );
        // 小写化改变字节长度时不 panic，只是不高亮。
        assert_eq!(
            split_hit("TİTLE", "ti"),
            (String::new(), String::new(), String::new())
        );
    }

    #[test]
    fn visible_index_follows_first_row() {
        assert_eq!(visible_index_label(0, 0), "1");
        assert_eq!(visible_index_label(8, 0), "9");
        assert_eq!(visible_index_label(9, 0), "");
        assert_eq!(visible_index_label(8, 8), "1");
        assert_eq!(visible_index_label(16, 8), "9");
        assert_eq!(visible_index_label(17, 8), "");
        assert_eq!(index_of_display(1, 8, 20), Some(8));
        assert_eq!(index_of_display(9, 8, 20), Some(16));
        assert_eq!(index_of_display(1, 0, 0), None);
    }

    #[test]
    fn hotkey_matches_exact() {
        let hk = crate::settings::Hotkey::new(crate::settings::MOD_CONTROL, 0xBD);
        assert!(hk.matches(crate::settings::MOD_CONTROL, 0xBD));
        assert!(!hk.matches(crate::settings::MOD_CONTROL | crate::settings::MOD_SHIFT, 0xBD));
        assert!(!hk.matches(crate::settings::MOD_CONTROL, 0xBB));
    }

    #[test]
    fn json_preview_pretty_prints_structure() {
        let (text, mono, extra) = format_preview_text(r#"{"a":1,"b":[true]}"#.into());
        assert!(mono);
        assert!(text.contains('\n'));
        assert!(text.contains("\"a\": 1"));
        let extra = extra.expect("json shape");
        assert!(extra.contains("对象"));
        assert!(extra.contains("2 个键"));
    }

    #[test]
    fn json_array_preview_summarizes_len() {
        let (text, mono, extra) = format_preview_text("[1,2,3]".into());
        assert!(mono);
        assert!(text.contains('\n'));
        let extra = extra.expect("array shape");
        assert!(extra.contains("数组"));
        assert!(extra.contains("3 项"));
    }

    #[test]
    fn plain_text_preview_keeps_content() {
        let (text, mono, extra) = format_preview_text("hello world".into());
        assert!(!mono);
        assert_eq!(text, "hello world");
        assert!(extra.is_none());
    }

    #[test]
    fn fill_inclusive_keeps_both_ends() {
        let mut set = BTreeSet::new();
        fill_inclusive(&mut set, 0, 4);
        assert_eq!(set.iter().copied().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
        fill_inclusive(&mut set, 3, 1);
        assert_eq!(set.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn row_picked_only_marks_multi() {
        let empty = BTreeSet::new();
        assert!(!row_picked(4, &empty, 4), "单选不写 picked");
        let single = BTreeSet::from([4]);
        assert!(!row_picked(4, &single, 4));
        assert!(!row_picked(4, &single, 0));
        let mut set = BTreeSet::new();
        fill_inclusive(&mut set, 0, 4);
        assert!(row_picked(4, &set, 0), "区间含起始行");
        assert!(row_picked(4, &set, 4), "区间含当前行");
        assert!(!row_picked(4, &set, 5));
    }

    #[test]
    fn well_formed_json_matches_strict_parse() {
        assert!(!is_well_formed_json(""));
        assert!(!is_well_formed_json("   "));
        assert!(!is_well_formed_json("not json"));
        assert!(!is_well_formed_json("{a:1}"));
        assert!(is_well_formed_json("{\"a\":1}"));
        assert!(is_well_formed_json(" [1, 2] "));
        assert!(is_well_formed_json("\"hi\""));
    }

    #[test]
    fn parse_menu_action_covers_wpf_and_extras() {
        assert!(matches!(parse_menu_action("paste"), Some(MenuAction::Paste)));
        assert!(matches!(parse_menu_action("batchall"), Some(MenuAction::BatchAll)));
        assert!(matches!(parse_menu_action("ocr"), Some(MenuAction::OcrPaste)));
        assert!(matches!(parse_menu_action("file"), Some(MenuAction::PasteAsFile)));
        assert!(matches!(parse_menu_action("json"), Some(MenuAction::PasteAsJson)));
        assert!(matches!(parse_menu_action("delete"), Some(MenuAction::Delete)));
        assert!(parse_menu_action("unknown").is_none());
    }

    #[test]
    fn adjacent_runs_group_text_vs_files() {
        use EntryKind::*;
        let ids = [
            (1, Text),
            (2, RichText),
            (3, Image),
            (4, Files),
            (5, Text),
        ];
        let segs = adjacent_paste_segs(&ids);
        assert_eq!(
            segs,
            vec![
                PasteSeg::MergeText(vec![(1, Text), (2, RichText)]),
                PasteSeg::MergeFiles(vec![(3, Image), (4, Files)]),
                PasteSeg::Single(5, Text),
            ]
        );
        let singles = adjacent_paste_segs(&[(9, Image)]);
        assert_eq!(singles, vec![PasteSeg::Single(9, Image)]);
    }
}
 