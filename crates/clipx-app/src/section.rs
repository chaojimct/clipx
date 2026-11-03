//! 统一配置窗的设置组注册表（全即时语义的数据层）。
//!
//! - 6 组：剪贴板 / 记录与检索 / 常规 / 文件夹跳转 / 高级 / 关于（旧设置窗已删，
//!   唯一配置面；左侧导航序号见单测锁定）。
//! - 每行即时生效：toggle/choice/action/slider 点了就落盘；text 每键尝试解析
//!   （合法即落盘，非法静默忽略、框里保留原文——与旧保存校验同口径，见 `CfgText` 表）。
//! - 开关在插件详情与本组双入口的（qf/fj/inline/ocr），走同一条 instant 路径，
//!   语义天然一致（旧草稿/即时双语义随旧窗删除而消失）。
//! - 全部能力已收敛到本窗：排除应用名单+进程拾取、穿透规则列表（含规则录制槽 100）、
//!   自定义对话框列表+删除、老版迁移卡片、短语编辑器（专用块）。
//! - 录制后端复用 `WinState`（无头调用，见 logic 注释）；更新状态读 `WinState`
//!   回投字段（同一份，不另建通道）。

use crate::settings::Settings;
use crate::settings_win::WinState;

/// 迁移清单（旧设置窗已删，此处清零锁死；单测数长度防回潮）。
#[allow(dead_code)]
pub const DEFERRED: &[&str] = &[];

/// 通用配置行（Slint 侧按 kind 分支渲染，见 plugin.slint）。
pub struct CfgRow {
    pub label: &'static str,
    pub help: &'static str,
    pub kind: CfgKind,
}

pub enum CfgKind {
    Toggle { key: &'static str, on: bool },
    Text { key: &'static str, value: String, width: i32 },
    Choice { key: &'static str, label: String },
    Action {
        key: &'static str,
        label: String,
        danger: bool,
        enabled: bool,
    },
    Info { text: String, warn: bool },
    /// 快捷键录制槽（slot 沿用 settings_win::SLOT_*，后端同一套）。
    Record { slot: i32, label: String, recording: bool },
    Slider { key: &'static str, value: f32 },
    /// 更新下载进度条（busy 时才出现；value<0 走不确定态，口径与旧窗一致）。
    Progress { value: f32 },
}

/// 导航分组（左栏三个 UiSection 的顺序；vec 顺序即全序号顺序）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavGroup {
    Settings,
    Plugins,
    Other,
}

pub struct SectionInfo {
    /// 组 id（导航/快照测试用；生产渲染只用 name/note/rows）。
    #[allow(dead_code)]
    pub id: &'static str,
    pub name: &'static str,
    /// 详情头注（右侧用，可长）。
    pub note: &'static str,
    /// 所属导航分组。
    pub group: NavGroup,
    /// 左侧圆点（插件行：ok/off/warn；设置组：""=不画）。
    pub dot: &'static str,
    /// 名下关联拓展包（PackDownload id；详情「关联拓展包」区渲染）。
    pub pack: Option<&'static str>,
    /// 密码库行专属解锁区。
    pub vault: bool,
    pub rows: Vec<CfgRow>,
    /// 短语编辑器专用块（仅 search 组；列表类通用行装不下选中+双字段）。
    pub phrases: bool,
}

/// 全部导航项（顺序即左栏顺序：设置 4 + 插件 5 + 其他 1）。
/// 增删改行同步改快照单测（顺序/分组/认领三锁）。
/// 全部导航项（顺序即左栏顺序：设置 4 + 插件 5 + 其他 1；分组只是展示切分）。
/// 增删改行同步改快照单测（顺序/分组/认领三锁）。
pub fn sections(
    s: &Settings,
    win: &WinState,
    settings_path: &std::path::Path,
    clear_armed: bool,
    vault_status: &str,
) -> Vec<SectionInfo> {
    vec![
        regular_section(s, clear_armed),
        clipboard_section(s, win),
        search_section(s),
        advanced_section(s, win),
        fj_plugin(s, win),
        qf_plugin(s),
        inline_plugin(s),
        vault_plugin(vault_status),
        ocr_plugin(s),
        about_section(s, win, settings_path),
    ]
}

fn tg(label: &'static str, help: &'static str, key: &'static str, on: bool) -> CfgRow {
    CfgRow {
        label,
        help,
        kind: CfgKind::Toggle { key, on },
    }
}

fn tx(
    label: &'static str,
    help: &'static str,
    key: &'static str,
    value: String,
    width: i32,
) -> CfgRow {
    CfgRow {
        label,
        help,
        kind: CfgKind::Text { key, value, width },
    }
}

fn ch(label: &'static str, help: &'static str, key: &'static str, label_text: String) -> CfgRow {
    CfgRow {
        label,
        help,
        kind: CfgKind::Choice {
            key,
            label: label_text,
        },
    }
}

fn act(label: &'static str, help: &'static str, key: &'static str, btn: String) -> CfgRow {
    CfgRow {
        label,
        help,
        kind: CfgKind::Action {
            key,
            label: btn,
            danger: false,
            enabled: true,
        },
    }
}

fn info(text: String) -> CfgRow {
    CfgRow {
        label: "",
        help: "",
        kind: CfgKind::Info { text, warn: false },
    }
}

fn rec(label: &'static str, slot: i32, s: &Settings, win: &WinState) -> CfgRow {
    let label_text = match slot {
        crate::settings_win::SLOT_MAIN => s.hotkey.display(),
        crate::settings_win::SLOT_BATCH => s.batch_hotkey.display(),
        crate::settings_win::SLOT_FJ => s.filejump_hotkey.display(),
        crate::settings_win::SLOT_PGUP => s.page_up.display(),
        crate::settings_win::SLOT_PGDN => s.page_down.display(),
        crate::settings_win::SLOT_RULE => "录制组合键…".to_string(),
        _ => String::new(),
    };
    CfgRow {
        label,
        help: "",
        kind: CfgKind::Record {
            slot,
            label: label_text,
            recording: win.recording() == slot,
        },
    }
}

fn clipboard_section(s: &Settings, win: &WinState) -> SectionInfo {
    SectionInfo {
        id: "clipboard",
        name: "剪贴板",
        note: "呼出、外观与粘贴行为。",
        pack: None,
        rows: vec![
            info("主面板：Ctrl+` 呼出 · ↑↓ 选择 · Enter 粘贴 · Space 预览 / 检索态分词 · Del 删除 · Ctrl+P 置顶 · Tab 切换模式。托盘图标左键单击 = 显示 / 隐藏。".to_string()),
            tg("剪贴板历史采集", "关则停新入库（托盘暂停/继续同效）", "capture", !crate::policy::is_capture_paused()),
            rec("呼出快捷键", crate::settings_win::SLOT_MAIN, s, win),
            rec("批量模式切换", crate::settings_win::SLOT_BATCH, s, win),
            rec("向上翻页", crate::settings_win::SLOT_PGUP, s, win),
            rec("向下翻页", crate::settings_win::SLOT_PGDN, s, win),
            ch("面板主键", "数字快贴 / 短语过滤", "panel", s.panel_key.clone()),
            tx("每次翻页条数", "1 ~ 50", "pageitems", s.popup_page_items.to_string(), 140),
            ch("外观主题", "", "theme", crate::settings_win::theme_label(&s.theme).to_string()),
            CfgRow {
                label: "不透明度",
                help: "",
                kind: CfgKind::Slider {
                    key: "opacity",
                    value: s.popup_opacity as f32,
                },
            },
            tx("面板宽度", "280 ~ 1200", "width", num_f(s.popup_width), 140),
            tx("面板最大高度", "200 ~ 900", "maxheight", num_f(s.popup_max_height), 140),
            tx("预览行数", "1 ~ 10", "preview", s.preview_max_lines.to_string(), 140),
            ch("弹出位置", "", "pos", crate::settings_win::pos_label(&s.popup_position).to_string()),
            ch("点击外部隐藏", "", "hide", crate::settings_win::hide_label(s.hide_on_click_outside).to_string()),
            ch("粘贴模式", "终端目标自动改用 Shift+Insert", "paste", crate::settings_win::paste_label(&s.paste_mode).to_string()),
            tg("双击才粘贴", "开 = 单击仅选中", "dblclick", s.paste_double_click),
            tg("粘贴后置顶", "开 = 贴过的条目排到最上（对齐老版）", "touch", s.paste_touch_top),
            tg("模拟粘贴", "写回后向目标窗发送粘贴键", "simulate", s.paste_simulate),
            tg("替换系统 Win+V", "", "winv", s.replace_win_v),
            tg("文本批量合并粘贴", "相邻纯文本合一段；关则逐条", "merge", s.batch_merge_text),
            tg("队列贴完后回普通模式", "", "autooff", s.batch_auto_off_when_empty),
        ],
        phrases: false,
        group: NavGroup::Settings,
        dot: "",
        vault: false,
    }
}

fn search_section(s: &Settings) -> SectionInfo {
    SectionInfo {
        id: "search",
        name: "记录与检索",
        note: "存多少、怎么找、快捷短语。",
        pack: None,
        rows: vec![
            tx("最大记录数", "10 ~ 100000", "max_items", s.max_items.to_string(), 140),
            tx("最大图片条数", "0 = 不限制，上限 5000", "max_images", s.max_image_items.to_string(), 140),
            tx("单图体积上限", "字节", "imgbytes", s.max_image_bytes.to_string(), 160),
            tg("全文深搜", "开则扫正文与 OCR", "deep", s.deep_search),
        ],
        phrases: true,
        group: NavGroup::Settings,
        dot: "",
        vault: false,
    }
}

fn regular_section(s: &Settings, clear_armed: bool) -> SectionInfo {
    SectionInfo {
        id: "regular",
        name: "常规",
        note: "启动、数据维护、危险操作。",
        pack: None,
        rows: vec![
            tg("开机自启动", "", "startup", s.run_at_startup),
            tg("以管理员身份启动", "每次启动弹 UAC", "admin", s.run_as_admin),
            info("导出文件是历史库的完整副本，可在另一台机器上用「导入历史」合并回来（按内容去重）。".to_string()),
            act("导出历史", "", "export", "导出历史…".to_string()),
            act("导入历史", "", "import", "导入历史…".to_string()),
            act("数据目录", "", "opendata", "打开数据目录".to_string()),
            tg("退出时清空历史", "仅清历史，短语保留", "clearexit", s.clear_history_on_exit),
            info("下面的操作不可撤销。清空只影响历史记录，快捷短语会保留。".to_string()),
            CfgRow {
                label: "历史记录",
                help: "两段确认，防误触",
                kind: CfgKind::Action {
                    key: "clear",
                    label: if clear_armed {
                        "再次点击确认清空".to_string()
                    } else {
                        "清空历史".to_string()
                    },
                    danger: clear_armed,
                    enabled: true,
                },
            },
        ],
        phrases: false,
        group: NavGroup::Settings,
        dot: "",
        vault: false,
    }
}

fn fj_plugin(s: &Settings, win: &WinState) -> SectionInfo {
    let mut rows = vec![
        tg("启用跳转", "关则不检测对话框", "fj", s.filejump_enabled),
        rec("跳转快捷键", crate::settings_win::SLOT_FJ, s, win),
        tx("列表延时", "0~10000 ms，延时内再按直跳预选项", "delay", s.filejump_show_delay_ms.to_string(), 140),
        tg("对话框打开时自动弹出", "", "autopopup", s.filejump_auto_popup),
        tg("自动跳转到最佳路径", "直跳首条，同框只跳一次", "autojump", s.filejump_auto_jump),
        tg("切回时自动同步路径", "", "autosync", s.filejump_auto_sync),
    ];
    if !s.filejump_auto_popup {
        rows.push(ch(
            "跳转列表跟随",
            "",
            "follow",
            crate::settings_win::follow_label(&s.filejump_follow_mode).to_string(),
        ));
    }
    rows.push(tg("Shell 注入跳转", "关则只模拟地址栏；WPS 恒不注入", "inject", s.filejump_shell_inject));
    rows.push(tg("全盘搜索补充", "当前目录优先，再补全盘命中", "evsearch", s.filejump_everything_search));
    rows.push(ch(
        "全盘搜索索引",
        "都没运行则用内置目录递归（深度 3）",
        "engine",
        crate::settings_win::engine_label(&s.filejump_search_engine).to_string(),
    ));
    rows.push(CfgRow {
        label: "",
        help: "",
        kind: CfgKind::Info {
            text: crate::settings_win::engine_status(&s.filejump_search_engine),
            warn: false,
        },
    });
    rows.push(tx("常用路径最大数量", "1 ~ 10", "recentmax", s.filejump_recent_max.to_string(), 140));
    rows.push(tx("自动加入常用阈值", "确认 N 次后入库", "addmin", s.filejump_auto_add_min.to_string(), 140));
    rows.push(info("内置识别不到的「打开 / 保存」窗口，按类名 + 进程名匹配。点「探测文件对话框」抓当前前台窗口，再把结果填进来。".to_string()));
    rows.push(act("探测文件对话框", "", "probe", "探测文件对话框".to_string()));
    rows.push(act("导入合并", "", "custom-import", "导入（合并）".to_string()));
    rows.push(act("导出全部", "", "custom-export", "导出全部".to_string()));
    rows.push(tx(
        "导入文件路径",
        "",
        "custom-path",
        win.custom_import_path().to_string(),
        200,
    ));
    rows.push(info(format!("存储文件：{}", win.custom_store_path().display())));
    rows.push(CfgRow {
        label: "",
        help: "",
        kind: CfgKind::Info {
            text: "窗口列表（删完即时生效）：".to_string(),
            warn: false,
        },
    });
    SectionInfo {
        id: "filejump",
        name: "文件夹跳转",
        note: "打开/保存框直跳目录；全部选项都在这里。",
        pack: None,
        rows,
        phrases: false,
        group: NavGroup::Plugins,
        dot: if s.filejump_enabled { "ok" } else { "off" },
        vault: false,
    }
}

fn qf_plugin(s: &Settings) -> SectionInfo {
    SectionInfo {
        id: "qf",
        name: "资源管理器快速查找",
        note: "资源管理器内键入即搜（Everything / FindX）。",
        pack: None,
        rows: vec![
            tg("启用快速查找", "关则资源管理器内打字无反应", "qf", s.explorer_everything_quickfind_enabled),
            ch("弹窗跟随", "", "qffollow", crate::settings_win::qffollow_label(&s.explorer_quickfind_follow_mode).to_string()),
            ch("选中操作", "", "qfopen", crate::settings_win::qfopen_label(&s.explorer_quickfind_open_mode).to_string()),
            tx("筛选最大条数", "1 ~ 2000", "qfmax", s.explorer_everything_quickfind_max_results.to_string(), 140),
        ],
        phrases: false,
        group: NavGroup::Plugins,
        dot: if s.explorer_everything_quickfind_enabled { "ok" } else { "off" },
        vault: false,
    }
}

fn inline_plugin(s: &Settings) -> SectionInfo {
    SectionInfo {
        id: "inline",
        name: "行内补全",
        note: "任意应用打字时跟随补全（实验性）。",
        pack: None,
        rows: vec![
            tg("启用行内补全", "收藏优先，其次常用；默认关", "inline", s.inline_enabled),
            tx("触发长度", "2 ~ 8 个字符", "inlinemin", s.inline_min_len.to_string(), 140),
            tx("候选条数", "1 ~ 8", "inlinemax", s.inline_max_items.to_string(), 140),
            tg("终端里也启用", "默认开；TUI 全屏程序里捣乱就关", "inlineterm", s.inline_in_terminal),
        ],
        phrases: false,
        group: NavGroup::Plugins,
        dot: if s.inline_enabled { "ok" } else { "off" },
        vault: false,
    }
}

fn ocr_plugin(s: &Settings) -> SectionInfo {
    SectionInfo {
        id: "ocr",
        name: "OCR 拓展",
        note: "截图图中文字可搜；精度引擎可下载。",
        pack: Some("ocr-models"),
        rows: vec![
            tg("图片文字识别", "关后停新图 OCR，旧结果保留", "ocr", s.image_ocr_enabled),
            ch("OCR 引擎", "自动=有精度引擎用精度引擎；切换需重启", "ocrengine", crate::settings_win::ocr_engine_label(&s.ocr_engine).to_string()),
        ],
        phrases: false,
        group: NavGroup::Plugins,
        dot: if s.image_ocr_enabled { "ok" } else { "off" },
        vault: false,
    }
}

/// 密码库圆点：缺 bw/未知 → 待处理；已锁定 → 中性闲置；已解锁 → 正常。
/// 按展示文案匹配（`vault_status_text` 的输出，空即未知）。
/// `pub(crate)`：密码库行已并入统一导航，plugin.rs 残留行复用同一份。
pub(crate) fn vault_dot(status: &str) -> &'static str {
    if status.is_empty() || status.starts_with("未检测") {
        "warn"
    } else if status == "已解锁" {
        "ok"
    } else if status == "已锁定" {
        "off"
    } else {
        "warn"
    }
}

fn vault_plugin(vault_status: &str) -> SectionInfo {
    SectionInfo {
        id: "vault",
        name: "密码库",
        note: "Bitwarden / Vaultwarden 只读填充；解锁在下方「解锁」区。",
        pack: Some("bw"),
        rows: vec![],
        phrases: false,
        group: NavGroup::Plugins,
        dot: vault_dot(vault_status),
        vault: true,
    }
}

fn advanced_section(s: &Settings, win: &WinState) -> SectionInfo {
    SectionInfo {
        id: "advanced",
        name: "高级",
        note: "按键穿透与掩码（含排除名单与规则列表）。",
        pack: None,
        rows: vec![
            tg("按键穿透", "面板开时把键交给外部钩子", "pt", s.passthrough_enabled),
            tg("穿透 Caps", "", "mask-caps", s.passthrough_mask & crate::settings::MOD_CAPS != 0),
            tg("穿透 Shift", "", "mask-shift", s.passthrough_mask & crate::settings::MOD_SHIFT != 0),
            tg("穿透 Ctrl", "", "mask-ctrl", s.passthrough_mask & crate::settings::MOD_CONTROL != 0),
            tg("穿透 Alt", "", "mask-alt", s.passthrough_mask & crate::settings::MOD_ALT != 0),
            tg("穿透 Win", "", "mask-win", s.passthrough_mask & crate::settings::MOD_WIN != 0),
            tg("保留面板导航键", "", "keepkeys", s.passthrough_keep_panel_keys),
            rec("穿透规则录制", crate::settings_win::SLOT_RULE, s, win),
        ],
        phrases: false,
        group: NavGroup::Settings,
        dot: "",
        vault: false,
    }
}

fn about_section(
    s: &Settings,
    win: &WinState,
    settings_path: &std::path::Path,
) -> SectionInfo {
    let data_dir = settings_path
        .parent()
        .unwrap_or(settings_path)
        .to_string_lossy()
        .into_owned();
    let busy = win.update_busy();
    let mut rows = vec![
        info("Rust + Slint 写成的 Windows 剪贴板管理器。前身是 WPF 版 ClipboardX —— 它的交互行为是本项目的规格书，老数据可在首次启动时自动导入。".to_string()),
        info(format!("当前版本：{}", env!("CARGO_PKG_VERSION"))),
        info(format!("构建：{}", crate::settings_win::build_label())),
        info(format!("数据目录：{data_dir}")),
        CfgRow {
            label: "",
            help: "",
            kind: CfgKind::Info {
                text: if win.update_status().is_empty() {
                    "更新：尚未检查".to_string()
                } else {
                    win.update_status().to_string()
                },
                warn: win.update_warn(),
            },
        },
    ];
    if busy {
        rows.push(CfgRow {
            label: "",
            help: "",
            kind: CfgKind::Progress {
                value: win.update_progress(),
            },
        });
    }
    rows.push(CfgRow {
        label: "检查更新",
        help: "",
        kind: CfgKind::Action {
            key: "check",
            label: if busy { "更新中…".to_string() } else { "检查更新".to_string() },
            danger: false,
            enabled: !busy,
        },
    });
    if win.update_available() && !busy {
        rows.push(act("下载并安装", "", "install", "下载并安装".to_string()));
    }
    rows.push(CfgRow {
        label: "下载页",
        help: "",
        kind: CfgKind::Action {
            key: "releases",
            label: "下载页".to_string(),
            danger: false,
            enabled: !busy,
        },
    });
    rows.push(act("项目主页", "", "repo", "项目主页".to_string()));
    rows.push(tg("启动时自动检查更新", "", "updates", s.check_updates));
    rows.push(tg(
        "发现新版本时自动下载并安装",
        "",
        "autoupdate",
        s.auto_update,
    ));
    rows.push(info("关闭自动安装时，发现新版本会提示你，点上方「下载并安装」才执行。".to_string()));
    SectionInfo {
        id: "about",
        name: "关于",
        note: "版本、更新、数据目录（含老版迁移卡片）。",
        pack: None,
        rows,
        phrases: false,
        group: NavGroup::Other,
        dot: "",
        vault: false,
    }
}

/// f64 设置值转输入框文本（整数不带小数点，与旧窗 `g()` 口径一致）。
fn num_f(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// 短语列表行文案（与旧窗快照同格式，见 settings_win）。
pub fn phrase_line(phrase: &str, content: &str) -> String {
    if phrase.is_empty() {
        format!("(空) → {}", crate::settings_win::truncate_p(content))
    } else {
        format!("{} → {}", phrase, crate::settings_win::truncate_p(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::default()
    }

    fn win() -> WinState {
        WinState::default()
    }

    fn secs() -> Vec<SectionInfo> {
        let dir = std::env::temp_dir();
        sections(&settings(), &win(), &dir.join("settings.json"), false, "已锁定")
    }

    #[test]
    fn ten_items_in_stable_order_and_groups() {
        let secs = secs();
        let ids: Vec<_> = secs.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![
                "regular",
                "clipboard",
                "search",
                "advanced",
                "filejump",
                "qf",
                "inline",
                "vault",
                "ocr",
                "about"
            ],
            "顺序即导航顺序：设置4 + 插件5 + 其他1，改了同步改快照"
        );
        let groups: Vec<_> = secs.iter().map(|s| s.group).collect();
        assert_eq!(
            groups,
            vec![
                NavGroup::Settings,
                NavGroup::Settings,
                NavGroup::Settings,
                NavGroup::Settings,
                NavGroup::Plugins,
                NavGroup::Plugins,
                NavGroup::Plugins,
                NavGroup::Plugins,
                NavGroup::Plugins,
                NavGroup::Other,
            ]
        );
        for s in &secs {
            assert!(!s.name.is_empty() && !s.note.is_empty(), "{} 文案", s.id);
            assert!(!s.dot.is_empty() || s.group != NavGroup::Plugins, "{} 插件行缺圆点", s.id);
            assert!(
                matches!(s.dot, "" | "ok" | "off" | "warn"),
                "{} dot 非法：{}",
                s.id,
                s.dot
            );
            for r in &s.rows {
                assert!(!r.label.is_empty() || matches!(r.kind, CfgKind::Info { .. } | CfgKind::Progress { .. }), "{} 有无名非展示行", s.id);
            }
        }
        // 密码库行是唯一空行组（详情只有解锁区+拓展包块）。
        assert!(secs.iter().find(|s| s.id == "vault").is_some_and(|s| s.rows.is_empty()));
    }

    #[test]
    fn plugin_rows_start_with_toggle() {
        // 开关位置统一：插件行第一行必须是启用开关（密码库除外，它没有开关）。
        for id in ["filejump", "qf", "inline", "ocr"] {
            let sec = secs().into_iter().find(|s| s.id == id).unwrap();
            assert!(
                matches!(sec.rows.first(), Some(CfgRow { kind: CfgKind::Toggle { .. }, .. })),
                "{id} 首行不是开关"
            );
        }
    }

    #[test]
    fn packs_claimed_by_single_owner() {
        // ocr-models 归 OCR 行，bw 归密码库行（各一个认领者，打错/漏认领即挂）。
        let secs = secs();
        let ocr_owners: Vec<_> = secs.iter().filter(|s| s.pack == Some("ocr-models")).collect();
        assert_eq!(ocr_owners.len(), 1);
        assert_eq!(ocr_owners[0].id, "ocr");
        let bw_owners: Vec<_> = secs.iter().filter(|s| s.pack == Some("bw")).collect();
        assert_eq!(bw_owners.len(), 1);
        assert_eq!(bw_owners[0].id, "vault");
        let dir = std::env::temp_dir();
        let packs = crate::plugin::packs(&dir);
        for id in ["ocr-models", "bw"] {
            assert!(packs.iter().any(|p| p.id == id), "packs() 缺 id={id}");
        }
    }

    #[test]
    fn vault_dot_maps_display_states() {
        assert_eq!(vault_dot(""), "warn");
        assert_eq!(vault_dot("未检测到 bw（请先安装 Bitwarden CLI）"), "warn");
        assert_eq!(vault_dot("已锁定"), "off");
        assert_eq!(vault_dot("已解锁"), "ok");
        assert_eq!(vault_dot("未来新状态"), "warn");
    }

    #[test]
    fn row_keys_are_known_actions() {
        // key 即 Cfg* arm 的匹配键；打错字点了没反应，用白名单锁死。
        let dir = std::env::temp_dir();
        // 两种形态都走一遍（follow 行只在 autopopup 关时出现）。
        // 去重断言只在单次 sections() 内有效，轮间做并集（follow 只出现一次也合法）。
        let mut keys = std::collections::HashSet::<String>::new();
        for autopopup in [true, false] {
            let mut st = settings();
            st.filejump_auto_popup = autopopup;
            let mut w = win();
            // install 行只在有可用更新时出现（与 UI 同条件）。
            w.set_update_available(true);
            let secs = sections(&st, &w, &dir.join("settings.json"), false, "已锁定");
            let mut once = std::collections::HashSet::<String>::new();
            collect_keys(&secs, &mut once);
            keys.extend(once);
        }
        // spot check：关键键必须存在。
        for k in ["capture", "dblclick", "theme", "opacity", "ocrengine", "startup", "clear", "fj", "follow", "inline", "qf", "pt", "check", "install", "updates", "qfmax", "custom-path", "probe", "panel", "pos", "hide", "paste", "engine", "qfopen", "qffollow"] {
            assert!(keys.contains(k), "缺键：{k}");
        }
    }

    fn collect_keys(secs: &[SectionInfo], keys: &mut std::collections::HashSet<String>) {
        for s in secs {
            for r in &s.rows {
                match &r.kind {
                    CfgKind::Toggle { key, .. }
                    | CfgKind::Text { key, .. }
                    | CfgKind::Choice { key, .. }
                    | CfgKind::Slider { key, .. } => {
                        assert!(keys.insert(key.to_string()), "重复键：{key}");
                    }
                    CfgKind::Action { key, .. } => {
                        assert!(keys.insert(key.to_string()), "重复键：{key}");
                    }
                    CfgKind::Record { slot, .. } => {
                        assert!(
                            matches!(*slot, 0 | 1 | 2 | 3 | 4 | 100),
                            "录制槽非法：{slot}"
                        );
                    }
                    CfgKind::Info { .. } | CfgKind::Progress { .. } => {}
                }
            }
        }
    }

    #[test]
    fn deferred_list_is_honest() {
        // 旧窗已删：迁移清单必须保持清零，加回任何项都算回潮。
        assert!(DEFERRED.is_empty());
    }

    #[test]
    fn phrase_line_matches_legacy_format() {
        assert_eq!(phrase_line("", "abcdef"), "(空) → abcdef");
        assert_eq!(phrase_line("w", "abcdef"), "w → abcdef");
    }
}
