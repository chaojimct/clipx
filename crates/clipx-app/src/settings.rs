//! 设置体系（对齐 WPF `AppSettings`，字段 1:1，JSON 兼容）。
//!
//! - 缺字段补默认、非法值 `normalize()` 归一（旧 JSON 可读）
//! - `validate()` 按 WPF `Save_Click` 顺序返回错误（设置窗口拒存用）
//! - 别名兼容 WPF PascalCase 字段（`MaxItems` 等），[`Settings::load`] 统一入口
//! - `FolderFavorite` 兼容旧 `Vec<String>` 形态（path only）

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// RegisterHotKey 修饰键（Win32 原值）+ 批量切换 Caps 扩展位（WPF `MOD_CAPS`）。
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_CAPS: u32 = 0x10000;

/// 可录制热键（修饰键 + 虚键码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    #[serde(default)]
    pub modifiers: u32,
    #[serde(default)]
    pub key: u32,
}

impl Hotkey {
    pub fn new(modifiers: u32, key: u32) -> Self {
        Self { modifiers, key }
    }

    /// 显示名（WPF `FormatHotkey`：Ctrl+Shift+Alt+Win+CapsLock+Key）。
    pub fn display(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.modifiers & MOD_CONTROL != 0 {
            parts.push("Ctrl");
        }
        if self.modifiers & MOD_SHIFT != 0 {
            parts.push("Shift");
        }
        if self.modifiers & MOD_ALT != 0 {
            parts.push("Alt");
        }
        if self.modifiers & MOD_WIN != 0 {
            parts.push("Win");
        }
        if self.modifiers & MOD_CAPS != 0 {
            parts.push("CapsLock");
        }
        let key_name = vk_name(self.key);
        if key_name.is_empty() {
            return parts.join("+");
        }
        if parts.is_empty() {
            return key_name.to_string();
        }
        format!("{}+{}", parts.join("+"), key_name)
    }

    /// 当前修饰 + 虚键是否命中本热键（精确匹配，含 Caps 物理按下位）。
    pub fn matches(self, modifiers: u32, vk: u32) -> bool {
        self.key == vk && self.modifiers == modifiers
    }
}

/// 虚键码显示名（常用全集；未知返回空）。
pub fn vk_name(vk: u32) -> &'static str {
    match vk {
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D => "Enter",
        0x1B => "Esc",
        0x20 => "Space",
        0x21 => "PgUp",
        0x22 => "PgDn",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x2C => "PrintScreen",
        0x2D => "Insert",
        0x2E => "Delete",
        0x30..=0x39 => match vk {
            0x30 => "0",
            0x31 => "1",
            0x32 => "2",
            0x33 => "3",
            0x34 => "4",
            0x35 => "5",
            0x36 => "6",
            0x37 => "7",
            0x38 => "8",
            _ => "9",
        },
        0x41..=0x5A => match vk {
            0x41 => "A",
            0x42 => "B",
            0x43 => "C",
            0x44 => "D",
            0x45 => "E",
            0x46 => "F",
            0x47 => "G",
            0x48 => "H",
            0x49 => "I",
            0x4A => "J",
            0x4B => "K",
            0x4C => "L",
            0x4D => "M",
            0x4E => "N",
            0x4F => "O",
            0x50 => "P",
            0x51 => "Q",
            0x52 => "R",
            0x53 => "S",
            0x54 => "T",
            0x55 => "U",
            0x56 => "V",
            0x57 => "W",
            0x58 => "X",
            0x59 => "Y",
            _ => "Z",
        },
        0x60..=0x69 => "Num",
        0x70..=0x87 => match vk {
            0x70 => "F1",
            0x71 => "F2",
            0x72 => "F3",
            0x73 => "F4",
            0x74 => "F5",
            0x75 => "F6",
            0x76 => "F7",
            0x77 => "F8",
            0x78 => "F9",
            0x79 => "F10",
            0x7A => "F11",
            0x7B => "F12",
            _ => "F",
        },
        0xBA => ";",
        0xBB => "=",
        0xBC => ",",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "`",
        0xDB => "[",
        0xDC => "\\",
        0xDD => "]",
        0xDE => "'",
        _ => "",
    }
}

/// 快捷短语（WPF `QuickPasteEntry`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickPaste {
    #[serde(default)]
    pub phrase: String,
    #[serde(default)]
    pub content: String,
}

/// 文件夹收藏（WPF `FolderFavoriteEntry`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FolderFavorite {
    Full {
        #[serde(default)]
        phrase: String,
        #[serde(default)]
        path: String,
    },
    /// 旧形态：纯路径字符串。
    PathOnly(String),
}

impl Default for FolderFavorite {
    fn default() -> Self {
        FolderFavorite::PathOnly(String::new())
    }
}

impl FolderFavorite {
    pub fn path(&self) -> &str {
        match self {
            FolderFavorite::Full { path, .. } => path,
            FolderFavorite::PathOnly(p) => p,
        }
    }

    pub fn phrase(&self) -> &str {
        match self {
            FolderFavorite::Full { phrase, .. } => phrase,
            FolderFavorite::PathOnly(_) => "",
        }
    }

    pub fn full(phrase: String, path: String) -> Self {
        FolderFavorite::Full { phrase, path }
    }
}

/// 按键穿透规则（WPF `KeyPassthroughRule`；key=0 表该修饰下任意键）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassthroughRule {
    #[serde(default)]
    pub modifiers: u32,
    #[serde(default)]
    pub key: u32,
}

impl PassthroughRule {
    pub fn display(&self) -> String {
        let hk = Hotkey::new(self.modifiers, self.key);
        if self.key == 0 {
            let base = hk.display();
            if base.is_empty() {
                return String::new();
            }
            return format!("{base}+*");
        }
        hk.display()
    }
}

fn default_true() -> bool {
    true
}
fn default_max_items() -> i64 {
    2000
}
fn default_max_image_items() -> i64 {
    150
}
fn default_qf_max_results() -> u32 {
    150
}
fn default_recent_max() -> usize {
    5
}
fn default_preview_lines() -> i64 {
    2
}
fn default_page_items() -> i64 {
    8
}
fn default_opacity() -> f64 {
    1.0
}
fn default_popup_w() -> f64 {
    420.0
}
fn default_popup_h() -> f64 {
    560.0
}
fn default_hotkey() -> Hotkey {
    Hotkey::new(MOD_CONTROL, 0xC0)
}
fn default_fj_hotkey() -> Hotkey {
    Hotkey::new(MOD_CONTROL, 0x47)
}
fn default_batch_hotkey() -> Hotkey {
    Hotkey::new(MOD_ALT, 0xBF)
}
fn default_page_up() -> Hotkey {
    Hotkey::new(MOD_CONTROL, 0xBD)
}
fn default_page_down() -> Hotkey {
    Hotkey::new(MOD_CONTROL, 0xBB)
}

/// 全量设置（字段对齐 WPF `AppSettings`；别名兼容 WPF JSON 名）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // ===== 剪贴板页 =====
    #[serde(default = "default_max_items", alias = "MaxItems")]
    pub max_items: i64,
    #[serde(default = "default_max_image_items", alias = "MaxImageItems")]
    pub max_image_items: i64,
    /// 隐藏项：单图上限（WPF `MaxImageSizeBytes`）。
    #[serde(default = "default_image_bytes", alias = "MaxImageSizeBytes")]
    pub max_image_bytes: u64,
    #[serde(default = "default_hotkey")]
    pub hotkey: Hotkey,
    #[serde(default = "default_batch_hotkey")]
    pub batch_hotkey: Hotkey,
    #[serde(default = "default_theme", alias = "Theme")]
    pub theme: String,
    #[serde(default = "default_caret", alias = "PopupPosition")]
    pub popup_position: String,
    #[serde(default = "default_true", alias = "HideOnSameAppClick")]
    pub hide_on_click_outside: bool,
    #[serde(default = "default_opacity", alias = "PopupOpacity")]
    pub popup_opacity: f64,
    #[serde(default = "default_preview_lines", alias = "PreviewMaxLines")]
    pub preview_max_lines: i64,
    /// WPF `PasteSimulationMode`；另保留 `paste_simulate` 总开关（Rust 扩展）。
    #[serde(default = "default_ctrlv", alias = "PasteSimulationMode")]
    pub paste_mode: String,
    #[serde(default = "default_true")]
    pub paste_simulate: bool,
    #[serde(default = "default_ctrl", alias = "PanelModifierKey")]
    pub panel_key: String,
    #[serde(default = "default_page_up")]
    pub page_up: Hotkey,
    #[serde(default = "default_page_down")]
    pub page_down: Hotkey,
    #[serde(default = "default_popup_w", alias = "PopupPanelWidth")]
    pub popup_width: f64,
    #[serde(default = "default_popup_h", alias = "PopupPanelMaxHeight")]
    pub popup_max_height: f64,
    #[serde(default = "default_page_items", alias = "PopupPageItems")]
    pub popup_page_items: i64,
    #[serde(default = "default_true", alias = "RunAtStartup")]
    pub run_at_startup: bool,
    #[serde(default = "default_true", alias = "RunAsAdministrator")]
    pub run_as_admin: bool,
    #[serde(default = "default_true", alias = "CheckUpdatesOnStartup")]
    pub check_updates: bool,
    #[serde(default, alias = "ReplaceSystemWinV")]
    pub replace_win_v: bool,
    #[serde(default = "default_true", alias = "BatchPasteMergeText")]
    pub batch_merge_text: bool,
    #[serde(
        default = "default_true",
        alias = "BatchQueueAutoSwitchToNormalAfterQueueDone"
    )]
    pub batch_auto_off_when_empty: bool,
    /// clipx 默认双击才粘贴（用户明确要求单击只选中）；WPF 默认是单击粘贴。
    #[serde(default = "default_true", alias = "PasteRequiresDoubleClick")]
    pub paste_double_click: bool,
    #[serde(default, alias = "ClearHistoryOnExit")]
    pub clear_history_on_exit: bool,
    #[serde(default = "default_true", alias = "ImageOcrEnabled")]
    pub image_ocr_enabled: bool,
    /// 全文深搜：默认只扫 preview/拼音，开则扫 full_text/OCR。
    #[serde(default, alias = "DeepSearchEnabled")]
    pub deep_search: bool,
    /// 隐藏项：已提示的更新 tag（WPF `LastStartupUpdateNotifiedTag`）。
    #[serde(default, alias = "LastStartupUpdateNotifiedTag")]
    pub last_update_tag: Option<String>,
    /// 运行时批量模式（WPF `BatchPasteMode`；设置窗无控件，持久化用）。
    #[serde(default = "default_off", alias = "BatchPasteMode")]
    pub batch_mode: String,
    /// 快捷短语（WPF `QuickPastes`）。
    #[serde(default, alias = "QuickPastes")]
    pub phrases: Vec<QuickPaste>,

    // ===== 跳转页 =====
    #[serde(default = "default_fj_hotkey")]
    pub filejump_hotkey: Hotkey,
    #[serde(default, alias = "FileJumpPickerShowDelayMs")]
    pub filejump_show_delay_ms: u64,
    #[serde(default = "default_dialog", alias = "FileJumpPickerFollowMode")]
    pub filejump_follow_mode: String,
    #[serde(default = "default_true", alias = "FileJumpPickerOpenWhenDialogForeground")]
    pub filejump_auto_popup: bool,
    #[serde(default, alias = "FileJumpAutoOnFirstClick")]
    pub filejump_auto_jump: bool,
    #[serde(default = "default_true", alias = "FileJumpAutoSyncOnReturn")]
    pub filejump_auto_sync: bool,
    #[serde(default = "default_true", alias = "EnableShellNavigateInject")]
    pub filejump_shell_inject: bool,
    #[serde(default = "default_true", alias = "FileJumpPickerEverythingFolderSearch")]
    pub filejump_everything_search: bool,
    #[serde(default = "default_recent_max", alias = "RecentFolderMaxCount")]
    pub filejump_recent_max: usize,
    #[serde(default = "default_min1", alias = "RecentFolderAutoAddMinCount")]
    pub filejump_auto_add_min: i64,
    /// 收藏（WPF `FolderFavorites`；兼容旧纯路径形态）。
    #[serde(default, alias = "FolderFavorites")]
    pub filejump_favorites: Vec<FolderFavorite>,
    /// 最近（WPF `RecentFileDialogFolders`，新在前）。
    #[serde(default, alias = "RecentFileDialogFolders")]
    pub filejump_recent: Vec<String>,
    /// 上次文件夹（WPF `LastFileDialogFolder`，与 recent[0] 同步）。
    #[serde(default, alias = "LastFileDialogFolder")]
    pub filejump_last: String,
    /// 确认计数（WPF `FolderConfirmCounts`）。
    #[serde(default, alias = "FolderConfirmCounts")]
    pub filejump_confirm_counts: HashMap<String, i64>,
    /// 总开关（Rust 门控；关则 Ctrl+G 与自动弹出全停）。
    #[serde(default = "default_true")]
    pub filejump_enabled: bool,

    // ===== 实验页 =====
    #[serde(default = "default_true", alias = "ExplorerEverythingQuickFindEnabled")]
    pub explorer_everything_quickfind_enabled: bool,
    #[serde(default = "default_explorer", alias = "ExplorerQuickFindOpenMode")]
    pub explorer_quickfind_open_mode: String,
    #[serde(
        default = "default_qf_max_results",
        alias = "ExplorerEverythingQuickFindMaxResults"
    )]
    pub explorer_everything_quickfind_max_results: u32,
    #[serde(default, alias = "KeyPassthroughEnabled")]
    pub passthrough_enabled: bool,
    #[serde(default = "default_caps", alias = "KeyPassthroughModifierMask")]
    pub passthrough_mask: u32,
    #[serde(default = "default_true", alias = "KeyPassthroughKeepPanelKeys")]
    pub passthrough_keep_panel_keys: bool,
    #[serde(default, alias = "KeyPassthroughRules")]
    pub passthrough_rules: Vec<PassthroughRule>,
    #[serde(default, alias = "ExclusionApps")]
    pub exclusion_apps: Vec<String>,
}

fn default_image_bytes() -> u64 {
    15 * 1024 * 1024
}
fn default_theme() -> String {
    "System".into()
}
fn default_caret() -> String {
    "Caret".into()
}
fn default_ctrlv() -> String {
    "CtrlV".into()
}
fn default_ctrl() -> String {
    "Ctrl".into()
}
fn default_off() -> String {
    "Off".into()
}
fn default_dialog() -> String {
    "Dialog".into()
}
fn default_explorer() -> String {
    "Explorer".into()
}
fn default_caps() -> u32 {
    MOD_CAPS
}
fn default_min1() -> i64 {
    1
}

impl Default for Settings {
    fn default() -> Self {
        serde_json::from_value(serde_json::Value::Object(Default::default()))
            .unwrap_or_else(|_| Self::fallback())
    }
}

impl Settings {
    fn fallback() -> Self {
        Self {
            max_items: default_max_items(),
            max_image_items: default_max_image_items(),
            max_image_bytes: default_image_bytes(),
            hotkey: default_hotkey(),
            batch_hotkey: default_batch_hotkey(),
            theme: default_theme(),
            popup_position: default_caret(),
            hide_on_click_outside: true,
            popup_opacity: default_opacity(),
            preview_max_lines: default_preview_lines(),
            paste_mode: default_ctrlv(),
            paste_simulate: true,
            panel_key: default_ctrl(),
            page_up: default_page_up(),
            page_down: default_page_down(),
            popup_width: default_popup_w(),
            popup_max_height: default_popup_h(),
            popup_page_items: default_page_items(),
            run_at_startup: true,
            run_as_admin: true,
            check_updates: true,
            replace_win_v: false,
            batch_merge_text: true,
            batch_auto_off_when_empty: true,
            paste_double_click: true,
            clear_history_on_exit: false,
            image_ocr_enabled: true,
            deep_search: false,
            last_update_tag: None,
            batch_mode: default_off(),
            phrases: Vec::new(),
            filejump_hotkey: default_fj_hotkey(),
            filejump_show_delay_ms: 0,
            filejump_follow_mode: default_dialog(),
            filejump_auto_popup: true,
            filejump_auto_jump: false,
            filejump_auto_sync: true,
            filejump_shell_inject: true,
            filejump_everything_search: true,
            filejump_recent_max: default_recent_max(),
            filejump_auto_add_min: 1,
            filejump_favorites: Vec::new(),
            filejump_recent: Vec::new(),
            filejump_last: String::new(),
            filejump_confirm_counts: HashMap::new(),
            filejump_enabled: true,
            explorer_everything_quickfind_enabled: true,
            explorer_quickfind_open_mode: default_explorer(),
            explorer_everything_quickfind_max_results: default_qf_max_results(),
            passthrough_enabled: false,
            passthrough_mask: default_caps(),
            passthrough_keep_panel_keys: true,
            passthrough_rules: Vec::new(),
            exclusion_apps: Vec::new(),
        }
    }

    /// 加载：缺字段补默认 → 旧字段迁移 → `normalize()`。
    pub fn load(path: &Path) -> Settings {
        let mut s: Settings = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        s.migrate_legacy();
        s.normalize();
        s
    }

    /// 旧 JSON 迁移（WPF `Load` 兼容语义）。
    fn migrate_legacy(&mut self) {
        // 旧纯路径收藏 current Settings 已由 untagged enum 兼容，无需动作。
        // LastFileDialogFolder 为空但 recent 非空 → 同步（WPF 反向迁移）。
        if self.filejump_last.is_empty() {
            if let Some(first) = self.filejump_recent.first().cloned() {
                self.filejump_last = first;
            }
        }
    }

    /// 非法值归一（WPF `Normalize` 语义；保存前也会调）。
    pub fn normalize(&mut self) {
        self.max_items = self.max_items.clamp(10, 100_000);
        self.max_image_items = self.max_image_items.clamp(0, 5000);
        self.preview_max_lines = self.preview_max_lines.clamp(1, 10);
        self.popup_width = finite_or(self.popup_width, default_popup_w()).clamp(280.0, 1200.0);
        self.popup_max_height =
            finite_or(self.popup_max_height, default_popup_h()).clamp(200.0, 900.0);
        self.popup_page_items = self.popup_page_items.clamp(1, 50);
        self.popup_opacity = finite_or(self.popup_opacity, 1.0).clamp(0.4, 1.0);
        self.filejump_show_delay_ms = self.filejump_show_delay_ms.min(10_000);
        self.filejump_recent_max = self.filejump_recent_max.clamp(1, 10);
        self.filejump_auto_add_min = self.filejump_auto_add_min.clamp(1, 100);
        self.explorer_everything_quickfind_max_results =
            self.explorer_everything_quickfind_max_results.clamp(1, 2000);
        if !["System", "Dark", "Light"].contains(&self.theme.as_str()) {
            self.theme = default_theme();
        }
        if !["Caret", "Mouse"].contains(&self.popup_position.as_str()) {
            self.popup_position = default_caret();
        }
        if !["CtrlV", "ShiftInsert"].contains(&self.paste_mode.as_str()) {
            self.paste_mode = default_ctrlv();
        }
        if !["Ctrl", "Alt", "Win", "CapsLock"].contains(&self.panel_key.as_str()) {
            self.panel_key = default_ctrl();
        }
        if !["Off", "Fifo", "Lifo"].contains(&self.batch_mode.as_str()) {
            self.batch_mode = default_off();
        }
        if self.filejump_follow_mode != "Mouse" {
            self.filejump_follow_mode = default_dialog();
        }
        if !["Explorer", "DirectOpen"].contains(&self.explorer_quickfind_open_mode.as_str()) {
            self.explorer_quickfind_open_mode = default_explorer();
        }
        // recent 去空去重（大小写不敏感保序），last 与 recent[0] 同步。
        let mut seen = std::collections::HashSet::new();
        self.filejump_recent.retain(|p| {
            let t = p.trim();
            !t.is_empty() && seen.insert(t.to_lowercase())
        });
        self.filejump_recent.truncate(self.filejump_recent_max);
        if let Some(first) = self.filejump_recent.first().cloned() {
            self.filejump_last = first;
        }
        // 收藏去空。
        self.filejump_favorites
            .retain(|f| !f.path().trim().is_empty());
        self.exclusion_apps.retain(|a| !a.trim().is_empty());
        // 上下翻页键相同 → 下翻回退默认（WPF 回退语义）。
        if self.page_up == self.page_down {
            self.page_down = default_page_down();
        }
    }

    /// 保存校验（WPF `Save_Click` 顺序；返回首个错误，空表通过）。
    /// 调用方展示全部错误并拒存。
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !(10..=100_000).contains(&self.max_items) {
            errs.push("最大记录数须在 10~100000".to_string());
        }
        if !(0..=5000).contains(&self.max_image_items) {
            errs.push("最大图片条数须在 0~5000（0=不限制）".to_string());
        }
        if !(1..=10).contains(&self.preview_max_lines) {
            errs.push("预览行数须在 1~10".to_string());
        }
        if !(280.0..=1200.0).contains(&self.popup_width) {
            errs.push("面板宽度须在 280~1200".to_string());
        }
        if !(200.0..=900.0).contains(&self.popup_max_height) {
            errs.push("面板最大高度须在 200~900".to_string());
        }
        if !(1..=50).contains(&self.popup_page_items) {
            errs.push("每次翻页条数须在 1~50".to_string());
        }
        if self.page_up == self.page_down {
            errs.push("向上/向下翻页快捷键不能完全相同".to_string());
        }
        if self.filejump_show_delay_ms > 10_000 {
            errs.push("跳转列表延时须在 0~10000".to_string());
        }
        if self.hotkey == self.filejump_hotkey {
            errs.push("呼出快捷键与跳转键不能相同".to_string());
        }
        if self.hotkey == self.batch_hotkey {
            errs.push("呼出快捷键与批量切换键不能相同".to_string());
        }
        if self.filejump_hotkey == self.batch_hotkey {
            errs.push("跳转键与批量切换键不能相同".to_string());
        }
        if !(1..=2000).contains(&self.explorer_everything_quickfind_max_results) {
            errs.push("筛选最大条数须在 1~2000".to_string());
        }
        if !(1..=10).contains(&(self.filejump_recent_max as i64)) {
            errs.push("常用路径最大数量须在 1~10".to_string());
        }
        if !(1..=100).contains(&self.filejump_auto_add_min) {
            errs.push("自动加入阈值须在 1~100".to_string());
        }
        errs
    }
}

fn finite_or(v: f64, d: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        d
    }
}

pub fn load(path: &Path) -> Settings {
    Settings::load(path)
}

pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut s = settings.clone();
    s.normalize();
    let json = serde_json::to_string_pretty(&s)?;
    std::fs::write(path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_wpf() {
        let s = Settings::default();
        assert_eq!(s.max_items, 2000);
        assert_eq!(s.max_image_items, 150);
        assert_eq!(s.hotkey, Hotkey::new(MOD_CONTROL, 0xC0));
        assert_eq!(s.filejump_hotkey, Hotkey::new(MOD_CONTROL, 0x47));
        assert_eq!(s.batch_hotkey, Hotkey::new(MOD_ALT, 0xBF));
        assert_eq!(s.theme, "System");
        assert!(s.hide_on_click_outside);
        assert!(s.batch_merge_text);
        assert!(s.batch_auto_off_when_empty);
    }

    #[test]
    fn normalize_clamps_and_sanitizes() {
        let mut s = Settings::default();
        s.max_items = 5;
        s.theme = "Neon".into();
        s.popup_opacity = 2.0;
        s.page_down = s.page_up;
        s.normalize();
        assert_eq!(s.max_items, 10);
        assert_eq!(s.theme, "System");
        assert_eq!(s.popup_opacity, 1.0);
        assert_ne!(s.page_down, s.page_up);
    }

    #[test]
    fn validate_catches_conflicts() {
        let mut s = Settings::default();
        s.filejump_hotkey = s.hotkey;
        let errs = s.validate();
        assert!(errs.iter().any(|e| e.contains("跳转键")));
    }

    #[test]
    fn legacy_favorites_migrate() {
        let v: Vec<FolderFavorite> =
            serde_json::from_str(r#"["C:\\a", {"phrase":"w","path":"C:\\b"}]"#).unwrap();
        assert_eq!(v[0].path(), "C:\\a");
        assert_eq!(v[1].phrase(), "w");
    }

    #[test]
    fn hotkey_display_names() {
        assert_eq!(Hotkey::new(MOD_CONTROL, 0xC0).display(), "Ctrl+`");
        assert_eq!(Hotkey::new(MOD_ALT, 0xBF).display(), "Alt+/");
        assert_eq!(Hotkey::new(MOD_CONTROL, 0xBD).display(), "Ctrl+-");
    }
}
