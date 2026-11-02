//! 内部插件注册表（Step 2b/3）：事件收敛 + trait 落地。
//!
//! - 事件包络：`AppEvt::Qf*` / `AppEvt::Fj*` 收成一个 `AppEvt::PluginEvt`
//!  （plugin id + [`PluginPayload`]）。inline 后续照此加分支，不再加顶层变体。
//! - trait：[`AppPlugin`]（`id()`）是注册表的锚点；`QfPlugin` 是第一个实现。
//!   注册表结构体（`Vec<Box<dyn AppPlugin>>` + 统一 `dispatch`）等 FjState
//!   所有权随 Step 4 的 State 改工一起迁移时再立——状态机还在 State 手里，
//!   硬立注册表就是伪通用（见 runtime-design §4）。
//! - keyboard_hook 的 `KeyEvt::*` 生产侧不动：钩子继续发键事件，
//!   `main.rs` 转发器/热键表包成包络，逻辑层按 id 路由。

/// 插件事件载荷。QF/FJ 事件原样搬运（字段不动），只换包络。
#[derive(Debug, Clone)]
pub enum PluginPayload {
    QfKey(crate::keyboard_hook::KeyEvt),
    QfRowActivated(i32),
    QfFolderResolved {
        gen: u64,
        folder: Option<String>,
    },
    QfResults {
        gen: u64,
        view: crate::explorer_quickfind::QfView,
    },
    // ===== 文件夹跳转（Step 3a：worker 回投收敛；函数体暂留 logic.rs 同名 helper，
    // 3c 随 fj_show/fj_activate 一起搬进 FjPlugin）=====
    /// 后台采集完成（gen 过期则丢弃）。
    FjCollected {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// 打字查询补充分阶段回投（gen 过期则丢弃）。
    FjEvResults {
        gen: u64,
        items: Vec<clipx_filejump::Candidate>,
    },
    /// 后台回读对话框当前目录（poll tick，不进按键路径）。
    FjFolder {
        dialog: isize,
        folder: Option<String>,
    },
    /// 后台导航完成（无条件复位 navigating，gen 过期也复位）。
    FjNavigated {
        gen: u64,
        path: String,
        ok: bool,
        is_file: bool,
    },
    // ===== 文件夹跳转 watcher/follow（Step 3b）=====
    /// 对话框前台自动弹出（watcher 线程，400ms 轮询 + ShowDelay 二次确认）。
    FjAuto(isize),
    /// WinEvent 实时跟随：对话框移动（跟随线程）。
    FjDialogMoved,
    /// WinEvent：对话框销毁（跟随线程）。
    FjDialogGone,
    // ===== 文件夹跳转触发与行动作（Step 3c）=====
    /// 全局 Ctrl+G / 托盘 / 热键表：切换 Picker（ FjToggle 经包络，见分发）。
    FjToggle,
    /// Picker 列表点击（行回调经包络）。
    FjRowActivated(i32),
    /// Picker header 手动拖拽（逻辑像素位移，经包络）。
    FjDragged(f32, f32),
    // ===== 行内补全（Step 1：键事件 + 行动，经包络；状态暂留主 State，
    // InlinePlugin 壳等交互冻结解除后立）=====
    /// 行内补全键（hook 观察到的键入/导航/确认/关闭，经 Key arm 转包络）。
    InlineKey(crate::keyboard_hook::KeyEvt),
    /// 行内小窗行点击补全。
    InlineActivate(i32),
    /// 行内小窗关闭。
    InlineDismissed,
}

/// 载荷类型名（诊断日志用，只记名不记内容——`InlineChar` 装着击键字符，
/// `QfResults` 装着路径，永远不许整包 `Debug` 落盘）。
/// 新增变体必须同步加分支（exhaustive match，编译器强制）。
pub fn payload_name(p: &PluginPayload) -> &'static str {
    match p {
        PluginPayload::QfKey(_) => "QfKey",
        PluginPayload::QfRowActivated(_) => "QfRowActivated",
        PluginPayload::QfFolderResolved { .. } => "QfFolderResolved",
        PluginPayload::QfResults { .. } => "QfResults",
        PluginPayload::FjCollected { .. } => "FjCollected",
        PluginPayload::FjEvResults { .. } => "FjEvResults",
        PluginPayload::FjFolder { .. } => "FjFolder",
        PluginPayload::FjNavigated { .. } => "FjNavigated",
        PluginPayload::FjAuto(_) => "FjAuto",
        PluginPayload::FjDialogMoved => "FjDialogMoved",
        PluginPayload::FjDialogGone => "FjDialogGone",
        PluginPayload::FjToggle => "FjToggle",
        PluginPayload::FjRowActivated(_) => "FjRowActivated",
        PluginPayload::FjDragged(..) => "FjDragged",
        PluginPayload::InlineKey(_) => "InlineKey",
        PluginPayload::InlineActivate(_) => "InlineActivate",
        PluginPayload::InlineDismissed => "InlineDismissed",
    }
}

/// 插件路由键。QF 用 `QfPlugin::ID`；FJ 壳（`FjPlugin`）/行内壳（`InlinePlugin`）
/// 落地前暂用常量，改名只改这一处。
pub const FILEJUMP_ID: &str = "filejump";
pub const INLINE_ID: &str = "inline";

/// 内部插件：`id()` 是路由键（分发按它匹配），也是审计日志的插件名；
/// `on_plugin_event()` 是包络分发唯一入口。`PluginCtx` 一次给齐四个分支
/// 所需（设置透传/回投通道/自家窗口弱引用/主窗可见性），插件不再直连 State。
pub trait AppPlugin {
    fn id(&self) -> &'static str;
    fn on_plugin_event(&mut self, payload: PluginPayload, ctx: &PluginCtx);
}

// ================= 插件管理（独立窗口的数据底座）=====================
//
// 设计：注册表是**现有开关的视图**，不是第二套开关。enabled 读写直通
// `Settings` 对应字段（经 `PluginToggle` 即时生效，与设置页草稿语义独立），
// 状态行复用各模块现成查询（vault 状态、OCR 引擎/模型、采集暂停位）。
// 新增插件只加行，不改分发。

/// 管理页一行插件：开关（有）/状态（有）/详情（去设置页）。
/// 键即 `toggle` 名（`SettingBool` 名）——注册表是现有开关的视图，不另立 id。
/// `config_note` 是右侧「配置」区的指引行：开关拨动之外的东西放这儿写，
/// 后续真配置行（如下拉/数值）也挂同一区，数据走同 struct 加字段。
pub struct PluginInfo {
    pub name: &'static str,
    pub desc: &'static str,
    pub enabled: bool,
    pub status: String,
    /// 对应 `SettingBool` 名（`handle_bool`）；None = 无开关（纯状态行）。
    pub toggle: Option<&'static str>,
    /// 右侧配置区指引（必须指到真实存在的设置位置，页名与 settings.slint 的 UiTab 一致）。
    pub config_note: &'static str,
}

/// 可下载拓展包一行：状态 + 动作（下载 / 看安装说明 / 无）。
/// 键即 `btn_cmd`（`pack:<id>` 走下载通道，`url:<addr>` 开浏览器）。
pub struct PackInfo {
    pub name: &'static str,
    pub desc: &'static str,
    pub state: String,
    pub btn_label: Option<&'static str>,
    pub btn_cmd: Option<&'static str>,
    /// 右侧配置区指引（下载状态机说明，同样可挂未来配置行）。
    pub config_note: &'static str,
}

/// 内置插件清单（顺序即管理页展示顺序）。
pub fn builtin_plugins(
    settings: &crate::settings::Settings,
    vault_status: &str,
) -> Vec<PluginInfo> {
    vec![
        PluginInfo {
            name: "剪贴板历史采集",
            desc: "持续记录剪贴板，进 SQLite 历史",
            enabled: !crate::policy::is_capture_paused(),
            status: if crate::policy::is_capture_paused() {
                "已暂停（托盘恢复）".to_string()
            } else {
                "采集中".to_string()
            },
            toggle: None,
            config_note: "暂停 / 恢复在托盘右键，无开关。",
        },
        PluginInfo {
            name: "Explorer 快速查找",
            desc: "资源管理器内键入即搜（Everything / FindX）",
            enabled: settings.explorer_everything_quickfind_enabled,
            status: if settings.explorer_everything_quickfind_enabled {
                "已启用".to_string()
            } else {
                "已关闭".to_string()
            },
            toggle: Some("qf"),
            config_note: "触发方式等更多选项在设置「高级」页「资源管理器快速查找」区。",
        },
        PluginInfo {
            name: "文件夹跳转",
            desc: "打开/保存框 Ctrl+G 直跳目录",
            enabled: settings.filejump_enabled,
            status: if settings.filejump_enabled {
                "已启用".to_string()
            } else {
                "已关闭".to_string()
            },
            toggle: Some("fj"),
            config_note: "快捷键与对话框规则在设置「文件夹跳转」页。",
        },
        PluginInfo {
            name: "行内补全（实验性）",
            desc: "任意应用打字时跟随补全",
            enabled: settings.inline_enabled,
            status: if settings.inline_enabled {
                "已启用".to_string()
            } else {
                "已关闭".to_string()
            },
            toggle: Some("inline"),
            config_note: "触发词长度等选项在设置「高级」页「行内补全」区。",
        },
        PluginInfo {
            name: "密码库",
            desc: "Bitwarden / Vaultwarden 只读填充；解锁在设置高级页",
            enabled: true,
            status: if vault_status.is_empty() {
                "未知".to_string()
            } else {
                vault_status.to_string()
            },
            toggle: None,
            config_note: "解锁在设置「高级」页「密码库」区；主密码只驻内存，永不落盘。",
        },
        PluginInfo {
            name: "OCR",
            desc: "截图图中文字可搜；精度包见左侧拓展包",
            enabled: settings.image_ocr_enabled,
            status: format!("引擎：{}", settings.ocr_engine),
            toggle: Some("ocr"),
            config_note: "引擎选择在设置「记录与检索」页「图片文字识别」区；精度包见左侧拓展包。",
        },
    ]
}

/// 拓展包清单。
pub fn packs(settings_path: &std::path::Path) -> Vec<PackInfo> {
    vec![ocr_pack_info(settings_path), bw_pack_info(settings_path)]
}

fn ocr_pack_info(settings_path: &std::path::Path) -> PackInfo {
    #[cfg(feature = "ocr-rapid")]
    {
        let dir = crate::ocr_pack::models_dir(settings_path);
        let assets = clipx_ocr::rapid::required_assets();
        let total = assets.len();
        let done = assets
            .iter()
            .filter(|(name, _, sha)| {
                dir.join(name).metadata().map(|m| m.len()).unwrap_or(0) > 0
                    && (sha.is_empty()
                        || crate::ocr_pack::file_sha256(&dir.join(name))
                            .is_some_and(|h| h.eq_ignore_ascii_case(sha)))
            })
            .count();
        let state = if clipx_ocr::rapid::models_ready(&dir) {
            "已下载".to_string()
        } else {
            format!("未下载（{done}/{total}）")
        };
        PackInfo {
            name: "OCR 精度拓展包",
            desc: "RapidOCR 模型（约 32MB），中文识别精度路径",
            state,
            btn_label: Some("下载 / 检查更新"),
            btn_cmd: Some("pack:ocr-models"),
            config_note: "下载完成后重启生效；进度走右下角提示条。",
        }
    }
    #[cfg(not(feature = "ocr-rapid"))]
    {
        let _ = settings_path;
        PackInfo {
            name: "OCR 精度拓展包",
            desc: "当前构建为标准版：点下载获取含精度包的完整版（覆盖安装，设置历史不动）",
            state: "未安装完整版".to_string(),
            btn_label: Some("下载完整版"),
            btn_cmd: Some("pack:ocr-models"),
            config_note: "与标准版同 AppId，原地升级；进度走右下角提示条。",
        }
    }
}

fn bw_pack_info(settings_path: &std::path::Path) -> PackInfo {
    if crate::bw_pack::is_ready(settings_path) {
        return PackInfo {
            name: "Bitwarden CLI",
            desc: "密码库填充的前置依赖（官方 OSS 构建，随 Data 存放）",
            state: format!("已安装（内建 {}）", crate::bw_pack::BW_VERSION),
            btn_label: None,
            btn_cmd: None,
            config_note: "内建包随 Data 走，搬家/重装自动跟随；也可在设置里填外部 bw 路径覆盖。",
        };
    }
    match clipx_vault::Vault::detect() {
        Some(p) => PackInfo {
            name: "Bitwarden CLI",
            desc: "密码库填充的前置依赖（官方维护）",
            state: format!("已安装（外部）：{}", p.display()),
            btn_label: None,
            btn_cmd: None,
            config_note: "用的是 PATH 里的外部 bw；也可点下载换内建包（版本 pin 死，更可预期）。",
        },
        None => PackInfo {
            name: "Bitwarden CLI",
            desc: "密码库填充的前置依赖（官方 OSS 构建，约 40MB，随 Data 存放）",
            state: "未安装".to_string(),
            btn_label: Some("下载"),
            btn_cmd: Some("pack:bw"),
            config_note: "下载官方 OSS 包并 SHA256 校验；完成后自动启用，进度走右下角提示条。",
        },
    }
}

/// 分发上下文（`logic.rs` 的 `PluginEvt` arm 构造，字段借用可与插件可变借用并存）。
pub struct PluginCtx<'a> {
    pub settings: &'a crate::settings::Settings,
    pub evt_tx: &'a std::sync::mpsc::Sender<crate::logic::AppEvt>,
    pub qf: &'a slint::Weak<crate::QuickFindWindow>,
    pub main_visible: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表锚点测试：toggle 名必须在 `handle_bool` 里存在，
    /// 否则管理页拨了开关没反应（两边改名必须同步）。
    #[test]
    fn registry_toggles_valid() {
        let settings = crate::settings::Settings::default();
        let plugins = builtin_plugins(&settings, "已锁定");
        assert!(plugins.len() >= 6);
        for p in &plugins {
            assert!(!p.name.is_empty() && !p.desc.is_empty());
            // 右侧配置区指引不能为空（空了右侧「配置」区就只剩一句空话）。
            assert!(!p.config_note.is_empty(), "{} 缺 config_note", p.name);
            if let Some(toggle) = p.toggle {
                // 与 settings_win::handle_bool 的 match 臂保持一致。
                assert!(
                    matches!(toggle, "qf" | "fj" | "inline" | "ocr"),
                    "未知开关名：{toggle}"
                );
                // 开关行必须有状态行（拨完用户要看得见）。
                assert!(!p.status.is_empty());
            }
        }
        // Toggle 名变更探测：handle_bool 侧加了新开关，注册表没加行就会剩。
        for toggle in ["qf", "fj", "inline", "ocr"] {
            assert!(
                plugins.iter().any(|p| p.toggle == Some(toggle)),
                "开关 {toggle} 在管理页无对应行"
            );
        }
    }

    #[test]
    fn packs_have_states_and_wellformed_actions() {
        let dir = std::env::temp_dir();
        let packs = packs(&dir);
        assert!(packs.len() >= 2);
        for p in &packs {
            assert!(!p.state.is_empty());
            assert!(!p.config_note.is_empty(), "{} 缺 config_note", p.name);
            match p.btn_cmd {
                None => assert!(p.btn_label.is_none(), "无动作就不能有按钮文案"),
                Some(c) => {
                    assert!(
                        c.starts_with("pack:") || c.starts_with("url:"),
                        "动作格式非法：{c}"
                    );
                    assert!(
                        p.btn_label.is_some_and(|s| !s.is_empty()),
                        "有动作必须有按钮文案"
                    );
                }
            }
        }
    }
    #[test]
    fn payload_names_are_stable_for_log_parsers() {
        // 日志只认这些名字；改名同步改排障文档。
        assert_eq!(
            payload_name(&PluginPayload::QfKey(crate::keyboard_hook::KeyEvt::Esc)),
            "QfKey"
        );
        assert_eq!(payload_name(&PluginPayload::FjToggle), "FjToggle");
        assert_eq!(
            payload_name(&PluginPayload::FjDialogMoved),
            "FjDialogMoved"
        );
        assert_eq!(
            payload_name(&PluginPayload::InlineDismissed),
            "InlineDismissed"
        );
        assert_eq!(
            payload_name(&PluginPayload::FjFolder {
                dialog: 0,
                folder: None
            }),
            "FjFolder"
        );
    }
}
