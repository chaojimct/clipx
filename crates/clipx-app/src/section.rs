//! 统一配置窗的设置组注册表（全即时语义的数据层）。
//!
//! - 6 组 1:1 对应旧设置页（剪贴板 / 记录与检索 / 常规 / 文件夹跳转 / 高级 / 关于），
//!   左侧导航跟在 6 插件之后（统一序号 6..11，`SECTION_OFFSET` 锁死，单测覆盖）。
//! - 每行即时生效：toggle/choice/action/slider 点了就落盘；text 每键尝试解析
//!   （合法即落盘，非法静默忽略、框里保留原文——与旧保存校验同口径，见 `CfgText` 表）。
//! - 开关在插件详情与本组双入口的（qf/fj/inline/ocr），走同一条 instant 路径，
//!   语义天然一致（旧草稿/即时双语义随旧窗删除而消失）。
//! - 暂留旧设置窗的：排除应用名单+进程拾取、穿透规则列表（含规则录制槽 100）、
//!   自定义对话框列表+删除、老版迁移卡片（见 `DEFERRED`）。短语编辑器已搬（专用块）。
//! - 录制后端复用 `WinState`（无头调用，见 logic 注释）；更新状态读 `WinState`
//!   回投字段（同一份，不另建通道）。

use crate::settings::Settings;
use crate::settings_win::WinState;

/// 组在统一序号里的起点（插件 0..6，设置组 6..12）。
pub const SECTION_OFFSET: usize = 6;

/// 暂留旧窗的清单（删窗前必须清零此处；`deferred_list_is_honest` 单测数行数）。
/// 仅测试引用（生产代码不读它——它是给人看的 checklist，不是逻辑开关）。
#[allow(dead_code)]
pub const DEFERRED: &[&str] = &[
    "排除应用名单+进程拾取",
    "穿透规则列表",
    "自定义对话框列表",
    "老版迁移卡片",
];

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

pub struct SectionInfo {
    /// 组 id（导航/快照测试用；生产渲染只用 name/note/rows）。
    #[allow(dead_code)]
    pub id: &'static str,
    pub name: &'static str,
    pub note: &'static str,
    pub rows: Vec<CfgRow>,
    /// 短语编辑器专用块（仅 search 组；列表类通用行装不下选中+双字段）。
    pub phrases: bool,
}

/// 6 组（顺序即导航顺序；增删组同步改 SECTION_OFFSET/总数单测）。
pub fn sections(
    s: &Settings,
    win: &WinState,
    settings_path: &std::path::Path,
    clear_armed: bool,
) -> Vec<SectionInfo> {
    vec![
        clipboard_section(s, win),
        search_section(s),
        regular_section(s, clear_armed),
        filejump_section(s, win),
        advanced_section(s),
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
        note: "呼出、外观与粘贴行为（旧「剪贴板」页）。",
        rows: vec![
            info("主面板：Ctrl+` 呼出 · ↑↓ 选择 · Enter 粘贴 · Space 预览 / 检索态分词 · Del 删除 · Ctrl+P 置顶 · Tab 切换模式。托盘图标左键单击 = 显示 / 隐藏。".to_string()),
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
    }
}

fn search_section(s: &Settings) -> SectionInfo {
    SectionInfo {
        id: "search",
        name: "记录与检索",
        note: "存多少、怎么找、快捷短语（旧「记录与检索」页）。",
        rows: vec![
            tx("最大记录数", "10 ~ 100000", "max_items", s.max_items.to_string(), 140),
            tx("最大图片条数", "0 = 不限制，上限 5000", "max_images", s.max_image_items.to_string(), 140),
            tx("单图体积上限", "字节", "imgbytes", s.max_image_bytes.to_string(), 160),
            tg("全文深搜", "开则扫正文与 OCR", "deep", s.deep_search),
            tg("图片文字识别", "关后停新图 OCR，旧结果保留", "ocr", s.image_ocr_enabled),
            ch("OCR 引擎", "自动=有拓展包用拓展包；切换需重启", "ocrengine", crate::settings_win::ocr_engine_label(&s.ocr_engine).to_string()),
        ],
        phrases: true,
    }
}

fn regular_section(s: &Settings, clear_armed: bool) -> SectionInfo {
    SectionInfo {
        id: "regular",
        name: "常规",
        note: "启动、数据维护、危险操作（旧「常规」页）。",
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
    }
}

fn filejump_section(s: &Settings, win: &WinState) -> SectionInfo {
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
    SectionInfo {
        id: "filejump",
        name: "文件夹跳转",
        note: "对话框跳转与自定义对话框（旧「文件夹跳转」页；窗口列表暂留旧窗）。",
        rows,
        phrases: false,
    }
}

fn advanced_section(s: &Settings) -> SectionInfo {
    SectionInfo {
        id: "advanced",
        name: "高级",
        note: "行内补全、快速查找、按键穿透（旧「高级」页；排除名单与规则列表暂留旧窗）。",
        rows: vec![
            tg("行内补全", "任意应用打字时跟随补全；收藏优先，其次常用", "inline", s.inline_enabled),
            tx("触发长度", "2 ~ 8 个字符", "inlinemin", s.inline_min_len.to_string(), 140),
            tx("候选条数", "1 ~ 8", "inlinemax", s.inline_max_items.to_string(), 140),
            tg("终端里也启用", "默认开；TUI 全屏程序里捣乱就关", "inlineterm", s.inline_in_terminal),
            tg("资源管理器快速查找", "当前文件夹内键入筛选", "qf", s.explorer_everything_quickfind_enabled),
            ch("弹窗跟随", "", "qffollow", crate::settings_win::qffollow_label(&s.explorer_quickfind_follow_mode).to_string()),
            ch("选中操作", "", "qfopen", crate::settings_win::qfopen_label(&s.explorer_quickfind_open_mode).to_string()),
            tx("筛选最大条数", "1 ~ 2000", "qfmax", s.explorer_everything_quickfind_max_results.to_string(), 140),
            tg("按键穿透", "面板开时把键交给外部钩子", "pt", s.passthrough_enabled),
            tg("穿透 Caps", "", "mask-caps", s.passthrough_mask & crate::settings::MOD_CAPS != 0),
            tg("穿透 Shift", "", "mask-shift", s.passthrough_mask & crate::settings::MOD_SHIFT != 0),
            tg("穿透 Ctrl", "", "mask-ctrl", s.passthrough_mask & crate::settings::MOD_CONTROL != 0),
            tg("穿透 Alt", "", "mask-alt", s.passthrough_mask & crate::settings::MOD_ALT != 0),
            tg("穿透 Win", "", "mask-win", s.passthrough_mask & crate::settings::MOD_WIN != 0),
            tg("保留面板导航键", "", "keepkeys", s.passthrough_keep_panel_keys),
        ],
        phrases: false,
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
        note: "版本、更新、数据目录（旧「关于」页；迁移卡片暂留旧窗）。",
        rows,
        phrases: false,
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

    #[test]
    fn six_sections_in_stable_order() {
        let dir = std::env::temp_dir();
        let secs = sections(&settings(), &win(), &dir.join("settings.json"), false);
        let ids: Vec<_> = secs.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec!["clipboard", "search", "regular", "filejump", "advanced", "about"],
            "组顺序即导航顺序，改了同步改快照"
        );
        assert_eq!(SECTION_OFFSET, 6, "插件 6 行之后才是组");
        for s in &secs {
            assert!(!s.name.is_empty() && !s.note.is_empty(), "{} 文案", s.id);
            assert!(!s.rows.is_empty(), "{} 空组", s.id);
            for r in &s.rows {
                assert!(!r.label.is_empty() || matches!(r.kind, CfgKind::Info { .. } | CfgKind::Progress { .. }), "{} 有无名非展示行", s.id);
            }
        }
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
            let secs = sections(&st, &w, &dir.join("settings.json"), false);
            let mut once = std::collections::HashSet::<String>::new();
            collect_keys(&secs, &mut once);
            keys.extend(once);
        }
        //  spot check：关键键必须存在。
        for k in ["dblclick", "theme", "opacity", "ocrengine", "startup", "clear", "fj", "follow", "inline", "qf", "pt", "check", "install", "updates", "qfmax", "custom-path", "probe"] {
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
                            matches!(*slot, 0 | 1 | 2 | 3 | 4),
                            "录制槽非法：{slot}（规则槽 100 暂留旧窗）"
                        );
                    }
                    CfgKind::Info { .. } | CfgKind::Progress { .. } => {}
                }
            }
        }
    }

    #[test]
    fn deferred_list_is_honest() {
        // 暂留旧窗的 4 项都在；清零一项就删一行，别留僵尸注释。
        assert_eq!(DEFERRED.len(), 4);
    }

    #[test]
    fn phrase_line_matches_legacy_format() {
        assert_eq!(phrase_line("", "abcdef"), "(空) → abcdef");
        assert_eq!(phrase_line("w", "abcdef"), "w → abcdef");
    }
}
