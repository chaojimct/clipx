//! 无头后端（旧设置窗口已删，见 git 历史）：录制槽、更新回投、自定义对话框、
//! vault 暂存——只留被统一窗口复用的部分，统统不碰 UI。
//!
//! - 录制后端（handle_record/slint_key/rel/vk）被统一窗口无头复用
//! - 自定义规则即时写盘（WPF：与点保存无关）
//! - 热键录制经 `keyboard_hook::RECORDING` + `RecordVk` 事件组修饰键

use std::path::PathBuf;
use std::sync::Mutex;

use slint::ComponentHandle;
use crate::settings::{Hotkey, PassthroughRule, Settings, MOD_ALT, MOD_CONTROL, MOD_SHIFT};

/// 录制槽位：0 呼出 / 1 批量 / 2 跳转 / 3 上翻 / 4 下翻 / 100 新穿透规则。
pub const SLOT_MAIN: i32 = 0;
pub const SLOT_BATCH: i32 = 1;
pub const SLOT_FJ: i32 = 2;
pub const SLOT_PGUP: i32 = 3;
pub const SLOT_PGDN: i32 = 4;
pub const SLOT_RULE: i32 = 100;

pub struct WinState {
    draft: Settings,
    recording: i32,
    /// Slint 录制期的修饰按住快照（裸修饰自反文本 \x10..\x12 去歧义用）。
    rec_held: u32,
    custom_rules: Vec<clipx_filejump::custom::CustomRule>,
    custom_path: PathBuf,
    import_path: String,
    /// 关于页「更新」区的实时状态（由 logic 层回投，不参与校验/保存）。
    upd_status: String,
    upd_busy: bool,
    upd_progress: f32,
    upd_available: bool,
    upd_warn: bool,
    /// 老版 WPF 迁移收尾卡片（检测到老版仍自启/运行时才显示）。
    legacy_visible: bool,
    legacy_has_autostart: bool,
    legacy_status: String,
    /// vault 主密码暂存（内存 only：统一窗口写入，解锁/保存/关闭即清；旧 SettingText("vault-pw") 写入，解锁/保存/
    /// 关闭即清，永不进 draft、不进快照之外的任何持久化）。
    vault_pw: String,
    /// vault 状态行（logic 层回投："已锁定/已解锁…/未检测到 bw"）。
    vault_status: String,
}

impl Default for WinState {
    fn default() -> Self {
        Self {
            draft: Settings::default(),
            // 录制槽 -1 才是空闲（0 是 SLOT_MAIN！derive 的 0 会让统一窗口误显示
            // 录制态，2026-11-03 实测抓获；旧窗因每次 open 重置才一直没爆）。
            recording: -1,
            rec_held: 0,
            custom_rules: Vec::new(),
            custom_path: PathBuf::new(),
            import_path: String::new(),
            upd_status: String::new(),
            upd_busy: false,
            upd_progress: 0.0,
            upd_available: false,
            upd_warn: false,
            legacy_visible: false,
            legacy_has_autostart: false,
            legacy_status: String::new(),
            vault_pw: String::new(),
            vault_status: String::new(),
        }
    }
}

impl WinState {
    /// 更新状态回投：驱动关于页「更新」区的状态行 / 进度条 / 按钮显隐。
    /// `progress < 0` = 不确定态（服务端未给 Content-Length）。
    pub fn set_update(&mut self, status: impl Into<String>, busy: bool, progress: f32, warn: bool) {
        self.upd_status = status.into();
        self.upd_busy = busy;
        self.upd_progress = progress;
        self.upd_warn = warn;
    }

    /// 是否有可用新版本（控制「下载并安装」按钮显隐）。
    pub fn set_update_available(&mut self, avail: bool) {
        self.upd_available = avail;
    }

    /// 更新区只读口（统一窗口「关于」组复用同一份回投状态，不另建通道）。
    pub fn update_status(&self) -> &str {
        &self.upd_status
    }
    pub fn update_busy(&self) -> bool {
        self.upd_busy
    }
    pub fn update_progress(&self) -> f32 {
        self.upd_progress
    }
    pub fn update_warn(&self) -> bool {
        self.upd_warn
    }
    pub fn update_available(&self) -> bool {
        self.upd_available
    }
    /// 录制槽（统一窗口复用 WinState 录制后端时读显；写走 handle_record）。
    pub fn recording(&self) -> i32 {
        self.recording
    }
    /// 自定义对话框导入路径框（无头复用时读显；写走 handle_text("custom-path")）。
    pub fn custom_import_path(&self) -> &str {
        &self.import_path
    }
    /// 导入路径写入（统一窗口侧会话级复用；旧 handle_text 同语义，不落盘）。
    pub fn set_import_path(&mut self, s: &str) {
        self.import_path = s.to_string();
    }
    /// 自定义对话框状态重载（统一窗口打开时调；与旧窗 open() 内联逻辑同源）。
    pub fn reload_custom(&mut self, settings_path: &std::path::Path) {
        let dir = settings_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        self.custom_path = dir.join("custom_file_dialogs.json");
        self.import_path = self.custom_path.to_string_lossy().to_string();
        self.custom_rules = clipx_filejump::custom::CustomStore::load(&self.custom_path).rules;
    }
    /// 自定义对话框列表展示行（统一窗口读显，与旧窗快照同格式）。
    pub fn custom_display_rows(&self) -> Vec<String> {
        self.custom_rules.iter().map(|r| r.summary()).collect()
    }
    /// 老版迁移卡片只读口（统一窗口读同一份回投，不另建通道）。
    pub fn legacy_visible(&self) -> bool {
        self.legacy_visible
    }
    pub fn legacy_status(&self) -> &str {
        &self.legacy_status
    }
    pub fn legacy_has_autostart(&self) -> bool {
        self.legacy_has_autostart
    }
    /// 自定义对话框存储路径展示。
    pub fn custom_store_path(&self) -> &std::path::Path {
        &self.custom_path
    }

    /// vault 主密码暂存清零（解锁/保存/打开/关闭后调用，永不落盘）。
    pub fn clear_vault_pw(&mut self) {
        self.vault_pw.clear();
    }

    /// vault 主密码写入（统一窗口密码框键入；旧 handle_text("vault-pw") 同语义）。
    pub fn set_vault_pw(&mut self, s: String) {
        self.vault_pw = s;
    }

    /// vault 主密码窥视（统一窗口回显用；取出用 take，读完不留痕靠调用方清）。
    pub fn peek_vault_pw(&self) -> &str {
        &self.vault_pw
    }

    /// 取出暂存主密码并清零（解锁消费唯一入口；调用后内存无残留）。
    pub fn take_vault_pw(&mut self) -> String {
        std::mem::take(&mut self.vault_pw)
    }

    /// 插件管理页用的 vault 状态行（设置页同一份回投值，无 vault 概念外泄）。
    pub fn vault_status_for_plugin(&self) -> &str {
        &self.vault_status
    }

    /// vault 状态行回投（logic 层根据 Vault::state() 设置）。
    pub fn set_vault_status(&mut self, s: impl Into<String>) {
        self.vault_status = s.into();
    }

    /// 老版 WPF 迁移收尾卡片回投：检测到老版仍自启/运行时显示，
    /// `has_autostart` 决定「停用自启」按钮是否可点。
    pub fn set_legacy_wpf(
        &mut self,
        visible: bool,
        has_autostart: bool,
        running: bool,
        exe: Option<&str>,
    ) {
        self.legacy_visible = visible;
        self.legacy_has_autostart = has_autostart;
        let mut t = if running {
            "老版 ClipboardX 正在运行".to_string()
        } else {
            "检测到老版 ClipboardX 仍设置为开机自启".to_string()
        };
        if has_autostart && running {
            t.push_str("，且仍设置为开机自启");
        }
        if let Some(p) = exe {
            t.push_str(&format!("（{p}）"));
        }
        t.push_str("。历史已导入到 clipx；建议停用它的自启，避免两套剪贴板监听并存。");
        self.legacy_status = t;
    }

}

pub(crate) fn theme_label(t: &str) -> &str {
    match t {
        "Dark" => "暗色",
        "Light" => "亮色",
        _ => "跟随系统",
    }
}
pub(crate) fn pos_label(t: &str) -> &str {
    if t == "Mouse" {
        "鼠标处"
    } else {
        "光标处"
    }
}
pub(crate) fn hide_label(b: bool) -> &'static str {
    if b {
        "任意点击隐藏"
    } else {
        "仅切换应用隐藏"
    }
}
pub(crate) fn paste_label(t: &str) -> &str {
    if t == "ShiftInsert" {
        "Shift+Insert"
    } else {
        "Ctrl+V"
    }
}
pub(crate) fn follow_label(t: &str) -> &str {
    if t == "Mouse" {
        "跟随鼠标"
    } else {
        "跟随对话框"
    }
}
pub(crate) fn qfopen_label(t: &str) -> &str {
    if t == "DirectOpen" {
        "直接打开"
    } else {
        "从资源管理器打开"
    }
}
pub(crate) fn qffollow_label(t: &str) -> &str {
    if t == "Explorer" {
        "跟随资源管理器"
    } else {
        "跟随鼠标"
    }
}
pub(crate) fn ocr_engine_label(t: &str) -> &str {
    match t {
        "media" => "系统引擎",
        "rapid" => "高精度拓展包",
        _ => "自动",
    }
}
pub(crate) fn engine_label(t: &str) -> &str {
    if t.eq_ignore_ascii_case("findx") {
        "FindX"
    } else {
        "Everything"
    }
}

/// 引擎可用性一行字（设置页只读展示）：让用户知道现在到底走哪条路。
pub(crate) fn engine_status(t: &str) -> String {
    let e = clipx_everything::Engine::parse(t);
    if clipx_everything::engine_available(e) {
        format!("{} 服务在运行", e.as_setting())
    } else {
        format!("{} 未运行 · 自动改用内置目录递归", e.as_setting())
    }
}

/// 项目仓库与发布页（设置「关于」页里的链接按钮）。
pub const REPO_URL: &str = "https://github.com/chaojimct/clipx";
pub const RELEASES_URL: &str = "https://github.com/chaojimct/clipx/releases";

/// 构建标识（关于页展示；排查时一眼看出手上是哪个产物）。
pub(crate) fn build_label() -> String {
    format!(
        "{} · {} {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

impl WinState {
    pub fn draft_settings(&self) -> &Settings {
        &self.draft
    }

    /// 自定义规则删除（即时写盘，WPF 语义）。返回是否成功。
    pub fn custom_remove(&mut self, idx: usize) -> bool {
        if idx >= self.custom_rules.len() {
            return false;
        }
        self.custom_rules.remove(idx);
        self.persist_custom().is_ok()
    }

    /// 自定义规则导入合并（即时写盘）。返回展示文案。
    pub fn custom_import(&mut self) -> String {
        let path = PathBuf::from(self.import_path.trim());
        let incoming = clipx_filejump::custom::CustomStore::load(&path);
        if incoming.rules.is_empty() {
            return format!("导入文件无规则：{}", path.to_string_lossy());
        }
        let mut store = clipx_filejump::custom::CustomStore { rules: self.custom_rules.clone() };
        let (added, replaced) = store.import_merge(incoming);
        self.custom_rules = store.rules;
        match self.persist_custom() {
            Ok(_) => format!("导入完成：新增 {added}，覆盖 {replaced}"),
            Err(e) => format!("写盘失败：{e}"),
        }
    }

    /// 自定义规则导出全部。返回展示文案。
    pub fn custom_export(&mut self) -> String {
        let path = PathBuf::from(self.import_path.trim());
        let store = clipx_filejump::custom::CustomStore { rules: self.custom_rules.clone() };
        match store.save(&path) {
            Ok(_) => format!("已导出 {} 条到 {}", store.rules.len(), path.to_string_lossy()),
            Err(e) => format!("导出失败：{e}"),
        }
    }

    fn persist_custom(&self) -> anyhow::Result<()> {
        clipx_filejump::custom::CustomStore { rules: self.custom_rules.clone() }
            .save(&self.custom_path)
    }




}

/// 快捷键录制槽切换（统一窗口无头复用；旧窗 open/页切注释已随旧窗删除）。
pub fn handle_record(st: &mut WinState, slot: i32) {
    if st.recording == slot {
        st.recording = -1;
    } else {
        st.recording = slot;
    }
    st.rec_held = 0;
    crate::win_popup::append_debug_log(
        "hotkey_debug.log",
        &format!(
            "record state slot={} (target={slot}) popup_visible={}",
            st.recording,
            crate::keyboard_hook::is_visible(),
        ),
    );
    crate::keyboard_hook::set_recording(st.recording);
}

/// Slint 侧录制按键（覆盖层 FocusScope 直收，不依赖低级钩子）。
/// `text` 为组合字符（Ctrl 组合是 \x01..\x1a 控制字符），`mods` 已是 MOD_* 位。
/// 返回 true=已消耗；F 键等无文本键返回 false，留给钩子兜底。
pub fn handle_slint_key(st: &mut WinState, text: &str, mods: u32, repeat: bool) {
    if st.recording < 0 {
        return;
    }
    if repeat {
        return; // 按住修饰的自动重复不是新键（否则按住 Ctrl 即误采 Ctrl+Q）
    }
    crate::win_popup::append_debug_log(
        "hotkey_debug.log",
        &format!(
            "slint-key slot={} text={:?} mods=0x{mods:02X} held=0x{:02X}",
            st.recording, text, st.rec_held,
        ),
    );
    if text == "\u{1b}" {
        // Esc=取消（与钩子路径一致）。
        handle_record_vk(st, 0x1B, mods);
        return;
    }
    let Some(vk) = slint_text_to_vk(st, text) else {
        return;
    };
    handle_record_vk(st, vk, mods);
}

/// Slint 侧松键：只维护裸修饰按住快照（捕获/取消后会话已结束，无影响）。
pub fn handle_slint_rel(st: &mut WinState, text: &str) {
    if st.recording < 0 {
        return;
    }
    let mut ch = text.chars();
    let (Some(c), None) = (ch.next(), ch.next()) else {
        return;
    };
    let bit = match c {
        '\x10' => MOD_SHIFT,
        '\x11' => MOD_CONTROL,
        '\x12' => MOD_ALT,
        _ => return,
    };
    st.rec_held &= !bit;
}

/// Slint KeyEvent.text → 虚键码（US 布局假设，与钩子侧 char_from_vk 一致）。
/// 裸修饰自反文本（\x10=Shift \x11=Ctrl \x12=Alt）：对应位按下前已按住才是
/// 字母键（Ctrl+Q 的 Q），否则是裸修饰 —— 标记按住并返回 None（WPF：纯修饰忽略）。
fn slint_text_to_vk(st: &mut WinState, text: &str) -> Option<u32> {
    let mut ch = text.chars();
    let c = ch.next()?;
    if ch.next().is_some() {
        return None;
    }
    if let Some(bit) = match c {
        '\x10' => Some(MOD_SHIFT),
        '\x11' => Some(MOD_CONTROL),
        '\x12' => Some(MOD_ALT),
        _ => None,
    } {
        if st.rec_held & bit != 0 {
            // 已按住 → 这是字母键（Ctrl+P/Q，Alt 罕见同理）。
            return Some(match c {
                '\x10' => 0x50,
                '\x11' => 0x51,
                _ => 0x52,
            });
        }
        st.rec_held |= bit;
        return None;
    }
    Some(match c {
        'a'..='z' => 0x41 + (c as u32 - 'a' as u32),
        'A'..='Z' => 0x41 + (c as u32 - 'A' as u32),
        '0'..='9' => 0x30 + (c as u32 - '0' as u32),
        ' ' | '\x00' => 0x20, // 后者=Ctrl+Space
        '\t' => 0x09,
        '\n' => 0x0D,
        '\u{8}' => 0x08,
        ';' | ':' => 0xBA,
        '=' | '+' => 0xBB,
        ',' | '<' => 0xBC,
        '-' | '_' => 0xBD,
        '.' | '>' => 0xBE,
        '/' | '?' | '\x1f' => 0xBF, // \x1f=Ctrl+/
        '`' | '~' => 0xC0,
        '[' | '{' => 0xDB,
        '\\' | '|' | '\x1c' => 0xDC, // \x1c=Ctrl+\
        ']' | '}' | '\x1d' => 0xDD, // \x1d=Ctrl+]
        '\'' | '"' => 0xDE,
        '^' | '\x1e' => 0x36, // \x1e=Ctrl+^（物理键 6）
        '\x01'..='\x1a' => 0x41 + (c as u32 - 1), // Ctrl+字母
        _ => return None,
    })
}

/// RecordVk 事件（Esc=取消；纯修饰/无修饰键忽略，停留录制态）。
/// `mods` 为钩子侧按下瞬间的快照（快按快松时逻辑层现读会读到空）。
pub fn handle_record_vk(st: &mut WinState, vk: u32, mods: u32) {
    if st.recording < 0 {
        return;
    }
    if vk == 0x1B {
        crate::win_popup::append_debug_log(
            "hotkey_debug.log",
            &format!("record slot={} vk=Esc cancelled", st.recording),
        );
        st.recording = -1;
        st.rec_held = 0;
        crate::keyboard_hook::set_recording(-1);
        return;
    }
    // 纯修饰键忽略（WPF）。
    if matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0x5B | 0x5C | 0xA0..=0xA5) {
        return;
    }
    if mods == 0 {
        crate::win_popup::append_debug_log(
            "hotkey_debug.log",
            &format!("record slot={} vk=0x{vk:02X} ignored(no-mod)", st.recording),
        );
        return; // 须含修饰键（WPF）
    }
    let hk = Hotkey::new(mods, vk);
    match st.recording {
        SLOT_MAIN => st.draft.hotkey = hk,
        SLOT_BATCH => st.draft.batch_hotkey = hk,
        SLOT_FJ => st.draft.filejump_hotkey = hk,
        SLOT_PGUP => st.draft.page_up = hk,
        SLOT_PGDN => st.draft.page_down = hk,
        SLOT_RULE => {
            if !st.draft.passthrough_rules.iter().any(|r| {
                r.modifiers == hk.modifiers && r.key == hk.key
            }) {
                st.draft.passthrough_rules.push(PassthroughRule {
                    modifiers: hk.modifiers,
                    key: hk.key,
                });
            }
        }
        _ => {}
    }
    crate::win_popup::append_debug_log(
        "hotkey_debug.log",
        &format!("record slot={} captured {}", st.recording, hk.display()),
    );
    st.recording = -1;
    st.rec_held = 0;
    crate::keyboard_hook::set_recording(-1);
}

pub(crate) fn truncate_p(s: &str) -> String {
    let t: String = s.chars().take(24).collect();
    if s.chars().count() > 24 {
        format!("{t}…")
    } else {
        t
    }
}

// ================= 主题 =================

/// 系统浅色判定（注册表 AppsUseLightTheme；取不到按深色）。禁止 spawn `reg.exe`。
#[cfg(windows)]
pub fn system_uses_light() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHED: AtomicU8 = AtomicU8::new(2);
    match CACHED.load(Ordering::Relaxed) {
        0 => return false,
        1 => return true,
        _ => {}
    }
    let light = read_apps_use_light();
    CACHED.store(if light { 1 } else { 0 }, Ordering::Relaxed);
    light
}

#[cfg(windows)]
fn read_apps_use_light() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
    };
    let mut dword: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut dword as *mut u32).cast()),
            Some(&mut size),
        )
    };
    err == ERROR_SUCCESS && dword == 1
}

#[cfg(not(windows))]
pub fn system_uses_light() -> bool {
    false
}

fn hex_color(s: &str) -> slint::Color {
    let h = s.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0xFF1E1E1E);
    let (a, r, g, b) = if h.len() > 6 {
        (
            (v >> 24) as u8,
            (v >> 16) as u8,
            (v >> 8) as u8,
            v as u8,
        )
    } else {
        (0xFF, (v >> 16) as u8, (v >> 8) as u8, v as u8)
    };
    slint::Color::from_argb_u8(a, r, g, b)
}

static LAST_THEME: Mutex<String> = Mutex::new(String::new());
static LAST_MODE: Mutex<String> = Mutex::new(String::new());

fn last_theme_name() -> String {
    LAST_THEME
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "System".into())
}

/// 给独立 Window 的 Theme 副本上色（设置 / FileJump / QuickFind 各一份）。
pub fn paint_theme_handle(t: crate::Theme) {
    let p = palette(&last_theme_name(), &last_mode_name());
    paint_theme(t, &p);
}

/// 批次模式主色（对齐 WPF `TrayIconSvg.GetModeMainHex`）。
fn mode_rgb(mode: &str) -> (u8, u8, u8) {
    match mode {
        "Fifo" => (0x25, 0x63, 0xEB),
        "Lifo" => (0xCA, 0x8A, 0x04),
        _ => (0x13, 0x94, 0x93),
    }
}

/// 按整数权重混合两色（对齐 WPF `MixRgbOnSolid`：截断除法，非四舍五入）。
fn mix_rgb(fg: (u8, u8, u8), bg: (u8, u8, u8), w_fg: u32, w_bg: u32) -> slint::Color {
    let d = w_fg + w_bg;
    let m = |f: u8, b: u8| ((f as u32 * w_fg + b as u32 * w_bg) / d) as u8;
    slint::Color::from_rgb_u8(m(fg.0, bg.0), m(fg.1, bg.1), m(fg.2, bg.2))
}

const LIGHT_WINDOW_BG: (u8, u8, u8) = (0xEF, 0xF1, 0xF5);
const DARK_EDITOR_BG: (u8, u8, u8) = (0x1E, 0x1E, 0x1E);

/// 19 项基色 + 第 20 项 separator。
///
/// `hover` / `selected` / `btn-hover` **不是静态色**：WPF 在
/// `PopupWindow.ApplyListSelectionBrushesForMode` 里按「当前批次模式主色 × 窗口底色」
/// 重算（浅色 5:20 / 10:15，深色 7:18 / 12:13）。此前这里直接抄了 ThemeManager 的
/// 静态初值（浅色 #E0F0EE / #CEE8E6），而 WPF 运行时早把它们覆盖掉了 —— 这就是选中行
/// 明显偏淡的原因。普通模式浅色下正确值 = 0.4×#139493 + 0.6×#EFF1F5 = #97CBCD。
fn palette(name: &str, batch_mode: &str) -> [slint::Color; 20] {
    let light = match name {
        "Light" => true,
        "Dark" => false,
        _ => system_uses_light(),
    };
    let mc = mode_rgb(batch_mode);
    let (hover, selected) = if light {
        (
            mix_rgb(mc, LIGHT_WINDOW_BG, 5, 20),
            mix_rgb(mc, LIGHT_WINDOW_BG, 10, 15),
        )
    } else {
        (
            mix_rgb(mc, DARK_EDITOR_BG, 7, 18),
            mix_rgb(mc, DARK_EDITOR_BG, 12, 13),
        )
    };
    let base: [&str; 19] = if light {
        [
            "#EFF1F5", "#E6E9EF", "#E0F0EE", "#CEE8E6", "#4C4F69", "#8C8FA1", "#9CA0B0",
            "#139493", "#BCC0CC", "#EFF1F5", "#E6E9EF", "#E0F0EE", "#FFFFFF", "#FFFFFF",
            "#D20F39", "#E0E3EB", "#EFF1F5", "#CCD0DA", "#C5C9D4",
        ]
    } else {
        [
            "#1E1E1E", "#252526", "#1B3F3F", "#245756", "#CCCCCC", "#9D9D9D", "#858585",
            "#139493", "#3E3E42", "#1E1E1E", "#252526", "#1B3F3F", "#252526", "#FFFFFF",
            "#F48771", "#1E1E1E", "#1E1E1E", "#252526", "#0C0C0C",
        ]
    };
    let mut p = [slint::Color::from_rgb_u8(0, 0, 0); 20];
    for (i, s) in base.iter().enumerate() {
        p[i] = hex_color(s);
    }
    p[2] = hover; // 列表悬停
    p[3] = selected; // 列表选中
    p[11] = hover; // btn-hover 与列表悬停同源（WPF 共用 HoverBrush）
    p[19] = hex_color(if light { "#CCD0DA" } else { "#3E3E42" });
    p
}

fn paint_theme(t: crate::Theme, p: &[slint::Color; 20]) {
    t.set_window_bg(p[0]);    t.set_surface(p[1]);
    t.set_hover(p[2]);
    t.set_selected(p[3]);
    t.set_primary_text(p[4]);
    t.set_secondary_text(p[5]);
    t.set_muted_text(p[6]);
    t.set_accent(p[7]);
    t.set_border(p[8]);
    t.set_popup_bg(p[9]);
    t.set_btn_bg(p[10]);
    t.set_btn_hover(p[11]);
    t.set_input_bg(p[12]);
    t.set_on_accent(p[13]);
    t.set_danger(p[14]);
    t.set_track(p[15]);
    t.set_header_bg(p[16]);
    t.set_footer_bg(p[17]);
    t.set_frame_bg(p[18]);
    t.set_separator(p[19]);
    // 卡片顶部内高光：亮底需要更实的一道白，暗底只需一丝，否则会糊成灰边。
    // 直接按窗口底色亮度判明暗——palette() 的签名不必为了这一个色位扩成 21 项。
    let light = p[0].red() > 128;
    let sheen_alpha = if light { 0x59 } else { 0x0F };
    t.set_card_sheen(slint::Color::from_argb_u8(sheen_alpha, 0xFF, 0xFF, 0xFF));
    // 搜索命中高亮：跳出色系用琥珀（Nord aurora 黄）。选中/悬停行的底是主色混出的青，
    // 青系高亮会整段融进去；黄在青底与灰底上都跳得出来，也和"匹配项"的通用语义一致。
    //   透明底(暗 #1E1E1E / 浅 #EFF1F5) → 12.2:1 / 5.3:1
    //   有底色(暗 selected #185656)      →  6.7:1 / 5.7:1
    let (hl, hl_fill) = if light {
        (hex_color("#8A5A00"), hex_color("#5C3A00"))
    } else {
        (hex_color("#FFD866"), hex_color("#FFE9A8"))
    };
    t.set_highlight(hl);
    t.set_highlight_on_fill(hl_fill);
    // 批量胶囊底色：跟批次模式主色（与托盘图标同源）。hover 压暗一档，
    // 否则 filled 胶囊 hover 时完全看不出反馈（原先 hover 与常态同色）。
    let mc = mode_rgb(&last_mode_name());
    t.set_batch_accent(slint::Color::from_rgb_u8(mc.0, mc.1, mc.2));
    let (hr, hg, hb) = darken_rgb(mc, 82);
    t.set_batch_accent_hover(slint::Color::from_rgb_u8(hr, hg, hb));
}

/// 按百分比压暗（对齐 WPF 主色 hover 的手感：约 0.82 倍）。
fn darken_rgb(c: (u8, u8, u8), pct: u32) -> (u8, u8, u8) {
    let f = |v: u8| ((v as u32 * pct) / 100) as u8;
    (f(c.0), f(c.1), f(c.2))
}

/// 即时应用主题（设置窗口循环/保存/取消回滚共用）。
pub fn apply_theme(name: &str, main_weak: &slint::Weak<crate::PopupWindow>) {
    if let Ok(mut g) = LAST_THEME.lock() {
        *g = name.to_string();
    }
    let p = palette(name, &last_mode_name());
    let weak = main_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        paint_theme(ui.global::<crate::Theme>(), &p);
    });
}

/// 记下当前批次模式：hover/selected 跟着模式主色走，独立 Window 也要取到同一份。
pub fn set_last_mode(mode: &str) {
    if let Ok(mut g) = LAST_MODE.lock() {
        *g = mode.to_string();
    }
}

fn last_mode_name() -> String {
    LAST_MODE
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "Off".into())
}

/// 批次模式切换后重刷列表配色（对齐 WPF `ApplyBatchModeChromeResources`）。
pub fn refresh_palette(main_weak: &slint::Weak<crate::PopupWindow>) {
    let p = palette(&last_theme_name(), &last_mode_name());
    let weak = main_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        paint_theme(ui.global::<crate::Theme>(), &p);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_defaults_idle() {
        // -1 才是空闲（0 = SLOT_MAIN）；derive 会给出 0，故手写 Default 并锁死。
        // 旧窗每次 open 重置所以没爆；统一窗口读原始 default，直接现形。
        assert_eq!(WinState::default().recording(), -1);
    }

    #[test]
    fn vault_pw_is_ephemeral() {
        // 主密码只进 WinState.vault_pw：take 取出即清；draft 里永无此字段。
        // （旧窗 handle_text("vault-pw") 已随旧窗删除，写入走 set_vault_pw。）
        let mut st = WinState::default();
        st.set_vault_pw("s3cr3t".to_string());
        assert_eq!(st.take_vault_pw(), "s3cr3t");
        assert!(st.vault_pw.is_empty(), "take 后内存无残留");
        // draft 侧无对应字段（编译期保证：Settings 无 vault_pw，它不是设置项）。
        let json = serde_json::to_value(&st.draft).unwrap_or_default();
        assert!(json.get("vault_pw").is_none());
        assert!(json.get("vault-pw").is_none());
    }
}
