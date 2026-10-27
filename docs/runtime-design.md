# clipx 输入增强 Runtime（能力级插件）设计

> 状态：v0.2 · 2026-10-27 · 配套文档：[ARCHITECTURE.md](ARCHITECTURE.md)（crate 结构与 ADR）、[CROSSPLATFORM.md](CROSSPLATFORM.md)（三平台原则与降级链）、[PRD.md](PRD.md)（范围）、[ROADMAP.md](ROADMAP.md)（里程碑）。v0.2 相对 v0.1 是整稿重写：立意从"整理现有代码"抬到"定义未来三年的平台"；新增统一数据模型、安全威胁模型（含一处已核实的现状日志隐私问题）、CLI/URI 一等触发器、AI/同步预留、生态与版本政策、可观测性。本文是重构的书面验收标准：评审通过后才写码。

## 0. 立意：我们在补操作系统缺的一层

### 0.1 为什么是 runtime，不是"剪贴板工具 Pro"

桌面 OS 的输入栈是断的：macOS/iOS 有系统级 AutoFill 与 Credential Provider，Windows **没有**——没有全应用可用的填充 API，没有统一的无障碍输入增强面，只有散装的剪贴板、钩子、UIA、TSF、剪贴板历史（Win+V）。每个输入增强想法（密码填充、快捷短语、翻译改写、文件跳转、行内补全）都要重新踩一遍 `SetWindowsHookEx`、焦点、注入、Electron 粘贴、输入法状态的坑。clipx 过去一年已经把这些坑踩完了（v0.10.6~13 全是这类修复）：**把踩坑的成果沉成可复用的底座，就是 runtime 的全部立意**。

三年后的图景：用户装 clipx 不是为了"剪贴板"，是为了"在任何输入框里获得超能力"；开发者（先是我们自己，未来是第三方）写输入增强插件时，不碰 Win32，只对 HostCaps 编程，一次编写、三平台运行（Wayland 降级见 §7）。

### 0.2 设计原则（排序即优先级）

1. **延迟是架构**：热键→首帧 <150ms、同步查询 <5ms、钩子回调零阻塞。任何让热路径变慢的设计，无论多优雅，一票否决。
2. **Host 吸收平台之痛**：平台分支永远住 host 后端，不住插件里（[CROSSPLATFORM.md §0](CROSSPLATFORM.md) 总原则的 runtime 版）。
3. **隐私是构造出来的**：秘密默认不落盘、不进索引、不进日志——靠接口形状保证，不靠自觉（§6）。
4. **窄腰模型**：插件与底座之间只用三样东西对话——Item（结果）、Action（动作）、Query/Trigger（查询与触发）。像 IP 之于互联网，腰越细，上下两端越自由（§2）。
5. **每步可发布**：单人节奏，每个里程碑结束都是可日用的完整产品（ROADMAP 纪律），重构步骤同理。

## 1. 现状：家底、乱麻、欠账

### 1.1 家底（能力 C1~C8，位置与教训）

| # | 能力 | 现状位置 | 硬骨头 |
|---|---|---|---|
| C1 | 无焦点浮层 | `clipx-app/src/win_popup.rs`（`WS_EX_NOACTIVATE`、跨帧抖尺寸修首帧残缺、`position_near_cursor`/`resolve_popup_anchor`）+ `tsf_inject.rs:133 measure_foreground_caret` | v0.10.6、v0.10.11~13 |
| C2 | 全局按键 | `keyboard_hook.rs`（`KeyEvt` 大枚举、50ms 消息泵、`ToUnicodeEx`、armed 会话态 + `heal_stuck_modifiers`、passthrough） | v0.10.5、v0.10.10 |
| C3 | 上下文感知 | `policy.rs:96 foreground_app`、`foreground_hwnd/foreground_exe_base/focus_is_password`、`explorer_shell`、`uia_dialog_folder`、来源应用记录 | — |
| C4 | 剪贴板读写 | `paste.rs` 四件套原子写（15ms×20 真实重试）、`write_text_for_target`、`gate.rs`、monitor 原子快照 | v0.10.9 |
| C5 | 粘贴/注入 | `send_paste` 双模、`inject.rs:58 navigate_to_folder`（退避 + COM 禁 Release）+ `navigate_keyboard` | M5 |
| C6 | 检索 | `store.search_ex`（窄表 3~5ms）、`inline.rs:75`、`everything::query`、`collectors`、`pinyin` 同构 | v0.10.9 |
| C7 | 反馈面 | toast 单通道单事、托盘 6 项、设置页、进度终态机 | v0.10.6/0.10.8 |
| C8 | 生命周期 | 设置 load/save/normalize、store 单写者 channel、无跨线程 Mutex、OCR 有界队列、单实例、自启 | CLAUDE.md 硬约束 |

### 1.2 乱麻（重构要解开的）

1. 主弹窗两条硬编码漏斗：`logic.rs:6292 merge_list_items`（短语负 ID + 历史硬拼）、`logic.rs:3439 write_entry_clipboard`（kind match）。`logic.rs:6223 is_phrase_id` 的 `id < 0` 约定加一个 vault 源就撞车。
2. 键盘开关散装：`keyboard_hook.rs` 内十几个 `set_*`（`set_qf_enabled/set_inline_enabled/set_fj_picker/set_visible…`），无中央路由表。
3. 三个独立列表栈：主弹窗 + QF（`explorer_quickfind.rs:61 QfState` + `AppEvt::Qf*`）+ FJ（`filejump.rs:37 FjState` + `AppEvt::Fj*` + watcher + 注入），窗口管理抄了三遍。
4. CLI 散装：`main.rs` 里 20 多个手写 `--flag` 判断（`--uitest/--snapshot/--qf-demo/--caret-probe/--import-wpf…`），无 `--help`、无结构化解析、无 `clipx://` URI。Wayland 时代 CLI 是唯一触发器（CROSSPLATFORM §1.3/1.5），今天这个形态接不住。
5. 设置快照按下标：`settings_win.rs:504 bools[0..19]`，中新开关必错位（CLAUDE.md 已点名）。

### 1.3 欠账（已核实，重构必须还）

1. **日志沾隐私**：`logic.rs:6015/6030` 把用户在任意应用键入的原文（`inline_buffer`）明文写进 `Data/inline_focus.log`，另有 `fg_info_line`（前台窗口信息）同行。全仓库 49 处 `write_data_log/append_debug_log` 调用从未过脱敏审计。§6.3 立项整改。
2. **崩溃无隔离**：panic 处理是全进程 hook 写 `Data/panic.log`（`main.rs:194`）；任何插件逻辑 panic = 全家闪退 + 可能丢剪贴板监听。§4.4 立项边界隔离。
3. **秘密无通道**：vault 秘密今天没有"不落库"的路可走（所有入库都经 `NewEntry` → SQLite）。§6.2 立项 ephemeral 通道。

## 2. 统一模型：Item / Action / Query / Trigger（窄腰）

现有 `EntryMeta` 是剪贴板形状的（id/kind/preview/pinned/created/source_app，`clipx-core/src/entry.rs:35`），装不下文件夹候选（path/alias/source）、vault 条目（uri/username/秘密引用）、命令结果。定义 runtime 级窄腰类型（进 `clipx-core`，纯数据、无行为）：

```rust
pub struct ItemId; // Db(i64) | Ext(&'static str, u64)，替代负 ID hack
pub struct Item {  // 所有插件结果的统一行
    pub id: ItemId,
    pub title: String,        // 主行（现 preview 的超集）
    pub subtitle: String,     // 来源/URI/路径等第二行（现无，新加）
    pub icon: IconRef,        // 类型图标/缩略图引用（懒加载，不常驻）
    pub score_hints: ScoreHints, // pinned/新鲜度/前台应用匹配度（供融合层）
    pub actions: Vec<ActionId>,  // 本行可用动作（见下）
    pub payload: PayloadRef,  // 懒取：Db 行 / 内存秘密引用（永不序列化到日志）
}

pub enum Action { // 动词与目标解耦
    Paste, Copy, Type,        // 三种输出方式（粘贴/复制/逐字打出，RDP 降级用 Type）
    Open, Reveal,             // 打开/定位（文件、URL、vault 条目）
    FillSecret,               // 秘密填充（备份剪贴板→写→Ctrl+V→恢复→清零，全走秘密通道）
    RunCommand,               // 执行命令式插件动作
    Pin, Delete, Edit, Transform(Vec<String>), // 管理与文本变换（翻译/改写/AI 预留）
}

pub struct QueryCtx<'a> {    // 统一查询上下文
    pub raw: &'a str,        // 含模式前缀（如 `bw `），由路由剥离
    pub tokens: Vec<&'a str>,// 空格交集分词（现有语义保留）
    pub filter: Option<Kind>, pub source: Option<&'a str>,
    pub deep: bool, pub limit: i64,
    pub foreground: ForegroundCtx, // 前台 exe/窗口类/是否密码框/是否终端/是否提权
}

pub enum Trigger {           // 统一触发器（CLI/URI 也是触发器，§7.3）
    Hotkey(HotkeyId), DialogForeground(Hwnd), ExplorerTyping(char),
    AnyTyping, TrayItem(&str), Startup, Cli(CliCmd), Uri(UriCmd), Timer(Interval),
}
```

配套三条语义（现状行为的制度化）：

- **融合排序**：多源结果进统一融合层（现 `rank_results` Schwartzian + `rank_score` 下沉，短语 +80/置顶 +40/字面/拼音/时间衰减权重保留），加两条新规则：前台应用匹配加成（抄 1Password Quick Access）、秘密行永不参与拼音模糊召回之外的全文索引。融合层是纯函数，单测锁死排序快照。
- **查询管线**：同步源（<5ms）首帧 → 异步源（IPC/子进程/网络）次帧流式回填 → 代际丢弃（QF `gen: AtomicU64` 模式推广为所有异步源的标准包络 `PluginEvt { plugin, gen, payload }`）。去抖（`refresh_input` 90ms）、输入停顿补刷保留。
- **动作目标感知**：`paste_mode_for_target` / `is_console_target` / 终端表（`paste.rs:408/432`，Cursor 不算终端的教训保留）收进 `FillCaps::resolve_target`，插件只说"我要输出这段文本"，host 决定 Ctrl+V / Shift+Insert / Type / 复制模式（含 Wayland 无注入降级）。

## 3. HostCaps：八个能力 + 三条铁律

```rust
pub struct HostCaps<'a> {
    pub ctx: &'a CtxCaps,       // C3：前台/密码框/排除/当前文件夹/对话框/提权与 RDP 标记
    pub popup: &'a PopupCaps,   // C1：无焦点窗、锚点、首帧重绘、透明度（host 拥有窗口壳，§4.2）
    pub keys: &'a KeysCaps,     // C2：订阅按键子集、吞/放行（不变量由 host 强制）
    pub clip: &'a ClipCaps,     // C4：读/原子写/秘密通道；Gate 由 host 代持，插件碰不到
    pub fill: &'a FillCaps,     // C5：输出三模 + 注入导航（门禁由 host 判，如 WPS 永不注入）
    pub search: &'a SearchCaps, // C6：history/inline/everything/collectors/pinyin，按预算分级
    pub ui: &'a UiCaps,         // C7：toast/托盘项/设置页注册（审批制）
    pub rt: &'a RtCaps,         // C8：后台 spawner、配置读写、结构化日志、探针
}
```

铁律（落地后写入 CLAUDE.md）：① 窗、钩、线程归 host，状态机归插件；② 插件回调只跑逻辑线程，同步 5ms/异步回投；③ 键路由中央化，吞 down 必吞 up 由 host 强制。新 crate `clipx-host` 只依赖 core/store/everything-filejump 的 UI-free 部分，零新增常驻依赖。

## 4. 运行时内核

### 4.1 插件 trait（两层）

```rust
pub trait CapabilityPlugin: Send {
    fn id(&self) -> &'static str;
    fn manifest(&self) -> Manifest;   // 权限 + 触发器 + 配置 schema（§6.1）
    fn on_trigger(&mut self, ev: Trigger, caps: &HostCaps) -> PluginAction;
    fn on_key(&mut self, k: KeyEvt, caps: &HostCaps) -> KeyDecision;
    fn on_plugin_evt(&mut self, p: Payload, caps: &HostCaps);
}
// 行级插件（history/phrases/vault/命令式源）是其退化：search + fill 两个方法。
```

首批内置插件：history、phrases、quickfind、filejump、inline、vault（Step 5）、commands（命令式通用源，vault 第一个吃）。QF/FJ 迁移即 Tier 2 接口的成型手段：先迁 QF（无注入，验"会话 + 代际 + 自有窗口"），再迁 FJ（验 watcher + 注入退避 + 防抖），迁完 `AppEvt` 的 QF/FJ 十几个变体收成一个 `PluginEvt`。

### 4.2 窗口：host 拥有壳，插件只供内容

四个 Slint 壳保留；定位/显隐/重绘/show 抖动进 `PopupCaps`；插件供行模型 + 按键处理器。自绘（OCR 选区、预览图层）走受限画布。spec 只抽象定位/显隐/重绘/透明度——抽象到刚好去掉重复。

### 4.3 路由表（中央）

热键（主/批量/FJ）→ history/filejump；对话框前台 watcher → filejump；Explorer 打字 → quickfind；任意键入（观察）→ inline；托盘/启动/CLI/URI/Timer → 订阅者。优先级与互斥（可见时主键互斥）语义不变，由 host 裁定。

### 4.4 隔离与可观测性

- **崩溃隔离**：插件回调边界 `catch_unwind`，panic 只停掉该插件（toast 告知 + 自动禁用 + 写 `panic.log` 附插件 id），进程不闪退。这是现状全进程 hook（`main.rs:194`）的升级。
- **后台监管**：`RtCaps::spawn` 统一登记后台线程（名、心跳、代际），看门狗（现有钩子看门狗模式推广）负责僵死 worker 的重启与上报。
- **日志分级与脱敏**：`Data/` 下诊断日志分三级——`debug.*`（开发用，默认关、可含定位信息）、`audit.log`（敏感动作结构化记录：WriteClip/SimulateKeys/InjectNav/秘密访问，只记元数据不记内容）、`panic.log`。§1.3 的 inline 明文击键日志作为 P0 整改：关闭原文记录，只留计数与状态机事件；全仓库 49 处日志调用过脱敏审计，`PayloadRef` 类型上 `Debug` 脱敏（秘密字段打印为 `•••`）。
- **探针与快照**：`--caret-probe` 模式推广为每能力的 `--probe=<cap>`（只读诊断）；`--uitest/--snapshot/--query/--qf-demo` 推广为插件快照矩阵（窗口 × Light/Dark × 态），进发版收尾清单。

## 5. 交互：统一入口与发现

- **统一入口**：主弹窗即命令面板。模式前缀（`bw ` 进 vault、`/` 进命令、空为历史）+ 前台应用匹配置顶（Quick Access 语义）+ Ctrl+1~9 快选 + 右键动作菜单（`MenuAction` 泛化为 `Action`）。新插件的发现成本 = 一个前缀，不用记新热键。
- **输出三模**：默认 Enter=填充/粘贴（减少秘密在剪贴板停留），Ctrl+Enter=复制 + 自动清除（15s），Alt+1/2/3=字段（用户/密码/TOTP），Tab 切 Type 模式。秘密行预览恒为 `•••`。
- **降级即 UX**：能力 `Unavailable` 时不展示触发器（Wayland 上没有 Ctrl+G，只有"复制路径"）；`Degraded` 时toast 一次性说明原因。绝不"点了没反应"。
- **权限引导**：macOS 辅助功能/Apple Events、开机自启提权，首次使用时一次引导；拒绝 = 对应能力降级，不弹裸错误。

## 6. 安全威胁模型

### 6.1 资产与对手

资产：剪贴板历史（含图片 OCR）、vault 秘密、用户键入流、文件路径、配置。对手模型：① 恶意/有 bug 的第三方代码（远期）——用权限 + 审计 + 隔离对付；② 恶意剪贴板内容/同步源——解析侧（docx/pptx/zip/pdf 全是手写解析，`clipx-doc`）保持纯 Rust、无 unsafe 图片解码由 `image` crate 承担，崩溃不跨进程；③ 本机其他用户/失窃设备——DB 静态加密（见 6.4）；④ 日志与残留——§4.4/6.3。

### 6.2 秘密端到端

vault 秘密走 ephemeral 通道：`zeroize` 内存、永不进 SQLite/FTS/日志/崩溃转储；填充=备份用户剪贴板→写秘密→Ctrl+V→~200ms 恢复→清零；TOTP 填充后自动续拷（Bitwarden 官方行为）。monitor 层先行两道门：剪贴板忽略格式（四标记）+ 密码管理器窗口（exe/标题），命中走内存不落库——即使不做填充功能，这也是必须还的安全债（Ditto 反面教材）。

### 6.3 日志与残留整改（P0，随 Step 0）

`inline_focus.log` 明文击键立即整改；49 处日志调用审计清单（哪处记了 query/buffer/路径/窗口标题，分级保留或脱敏）；`PayloadRef` 脱敏 `Debug`；崩溃转储不含秘密内存（审计 `panic.log` 写入点）。

### 6.4 预留：静态加密

PRD 曾以"与密码管理器职责重叠"排除剪贴板加密。runtime 立场：历史库静态加密（SQLCipher 或 age，opt-in，默认关）与密码管理器不重叠（防的是设备失窃，不是管密码），列为 Step 5 之后的候选，不在本次范围，但 `Store` 接口预留 `open_encrypted` 扩展点，`ItemId::Db` 不受影响。

## 7. 跨平台（自包含）

### 7.1 总原则（沿用 CROSSPLATFORM.md §0，复述为 runtime 约束）

1. 每平台做到该平台的最优形态，不逐字节一致；交互语义对齐（呼出/搜索/粘贴/跳转），实现各走最优路径。
2. Windows 现有路径冻结为基线；新平台代码全部进 `#[cfg]` 门控的平台模块，禁止改动 Windows 行为。
3. 搜索后端统一 findx2-ipc（JSON 行协议，命名管道/UDS 三平台同构）；Everything WM_COPYDATA 降级为 Windows 无 FindX 时的回退。
4. 权限前置：macOS 辅助功能是键盘监听、粘贴模拟、对话框跳转的共同依赖，M6 首个 spike 就申请。
5. 内存硬约束 ≤30MB 在 mac 上单独实测（Vision/objc 拉起风险），超标则 OCR 独立进程化。

### 7.2 能力 × 平台矩阵

| 能力 | Windows（基线，冻结） | macOS | Linux X11 | Wayland |
|---|---|---|---|---|
| C1 无焦点浮层 | `WS_EX_NOACTIVATE`（现有） | **最高风险**：winit 默认 NSWindow 会激活，需 objc2 运行时改造成 `NONACTIVATING_PANEL`（M6a spike 先验）；失败则退化为"呼出瞬间获焦"模型，记 PRD §8 | 面板正常取焦即可，失焦关闭已有 | 同 X11；全局热键改桌面快捷键绑 CLI（§7.3） |
| C2 全局按键 | `WH_KEYBOARD_LL`，面板永不获焦 | 面板当 keyWindow 收键 + 全局热键呼出；NSEvent 全局监听只做"Esc/点外关闭"类增强；打字拦截无等价物 | global-hotkey 呼出 + 面板取焦 | 无；桌面快捷键 + CLI 无头动作 |
| C3 上下文感知 | 前台 exe/hwnd、密码框检测、#32770 对话框、来源应用记录 | 辅助功能权限 + Apple Events（Finder 当前路径）+ AX 检测 NSOpenPanel/NSSavePanel（系统统一控件，比 #32770 简单） | WM_CLASS/标题启发式 + xdotool/DBus | 不做检测（对应能力 `Unavailable`，触发器隐藏） |
| C4 剪贴板 | clipboard-rs 事件驱动；DIB+PNG 双写；HTML Format 私有格式 | changeCount 500ms 轮询（系统无事件 API）；NSPasteboard PNG+TIFF；HTML 即 text/html，归一化进 kind=3 | clipboard-rs x11；PNG + text/uri-list；WPS/LibreOffice 逐家实测 | wl-clipboard-rs（ext-data-control）监听 |
| C5 粘贴/注入 | SendInput + ShellNavigate DLL 注入（冻结，COM 借用指针禁 Release） | CGEvent Cmd+V；"注入"改 AX 对目标路径元素直接 SetValue（最稳）→ 键盘链 Cmd+Shift+G → Return | XTest Ctrl+V；GTK 对话框 Ctrl+L → 路径 → Return；Qt 非原生对话框 fileNameEdit 直键入 | **无键盘注入**（平台安全模型）：只"写剪贴板 + 用户自粘"，`fill` 自动切复制模式 |
| C6 检索 | Everything IPC + FindX 双引擎 | findx2 UDS（`~/Library/Application Support/FindX/findx2.sock`，协议同构零改动）；未运行提示 `-startup` 拉起 | findx2 UDS（`$XDG_RUNTIME_DIR/findx2.sock`） | 同 X11 |
| C6 跳转 | Shell COM Navigate / SHOpenFolderAndSelectItems | `open -R`（Finder 显示）/ `open` | `nautilus --select` / `dolphin --select` / `xdg-open` | 复制路径 |
| OCR | Media OCR 直调 | Vision（`VNRecognizeTextRequest`，accurate + hw/zh），同一 `OcrEngine` trait | Tesseract（AppImage 捆 tessdata + chi_sim 可选下载）或 RapidOCR 替代 | 同 X11 |
| 热键/托盘 | global-hotkey + tray-icon（现有） | 同左；面板 keyWindow 模型（§7.1.1 风险） | global-hotkey；tray GTK loop（超标则直连 StatusNotifierItem） | tray-icon 同左；热键无 |
| 自启 | schtasks XML | SMAppService（LoginItems） | `.desktop` 进 `~/.config/autostart` | 同 X11 |
| 更新/打包 | Inno Setup（含双 DLL）；GitHub Releases 静默检查 | dmg（`LSUIElement=0`）；Developer ID + 公证，无账号先未签名（右键打开）；Sparkle 不引入，保持现有检查面 | deb + AppImage；更新同左 | 同 X11 |

### 7.3 触发器归一（同一插件，三平台三种入口 → 同一个 Trigger）

- QF：Win「Explorer 打字拦截」（钩子）/ mac+Linux「热键呼出浮层」/ Wayland「桌面快捷键调 `clipx --qf`」→ 均为 `Trigger::QuickFind`。跨平台形态差异的只是入口，浮层 UI 与结果跳转逻辑复用。
- FJ：Win「对话框前台 watcher + 注入」/ mac「AX Set + 键盘链」/ Linux「Ctrl+L 链」/ Wayland「复制路径」→ 均为 `Trigger::JumpToFolder{folder}`，host 按「注入→AX→键盘链→复制路径」选链（含 WPS 永不注入的门）。
- CLI/URI 一等触发器：重写 CLI 面——结构化解析、`--help`、机器可读 `--json`、`clipx://run?plugin=&action=&q=` URI、`--qf/--fill/--copy` 无头动作（供 Wayland 桌面快捷键与外部应用集成）。现有 20 多个调试 flag 收编为 `--dev-*` 命名空间，行为不变对照表逐项验。

### 7.4 平台 trait 与后端（Step 0 立骨架，M6/M7 填肉）

`KeySource`（钩子/keyWindow/取焦/CLI 四形态）、`PlatformPaste`（SendInput/CGEvent/XTest/无）、`PlatformPopup`（NoActivate/Nonactivating/取焦）、`PlatformDialog`（注入/AX Set/键盘链/复制路径）、`monitor::platform`（已有 trait，非 Windows 补实现）、`OcrEngine`（已有 trait，Vision/Tesseract 补实现）。Windows 实现 = 现有代码平移；非 Windows 先 stub（`tier()==Unavailable`，沿用 `win_popup.rs` 的 `#[cfg(not(windows))]` 空实现惯例）。**trait 形状 Step 0 冻结**，M6/M7 只填后端，插件侧零改动。

### 7.5 权限、签名、CI（过路费，一次列齐）

| 平台 | 权限 | 申请时机 | 签名/打包 | CI |
|---|---|---|---|---|
| Windows | 提权（自启/注入，按需 `--restart` 拉起） | 设置开启/首次注入时 | Inno；可选 codesign；`cdylib` 永不引入所以公证面干净 | 现有 |
| macOS | 辅助功能（键盘/CGEvent/AX）+ Apple Events（Finder）+ 登录项 | M6a 首次引导一次讲清；拒绝=能力降级 | Developer ID + 公证；无账号先未签名 | `cargo check --target aarch64-apple-darwin` 进日常 CI |
| Linux | 无特殊（X11）； portals/DBus 按需 | 首次使用时 | deb + AppImage（捆 tesseract + tessdata） | `cargo check --target x86_64-unknown-linux-gnu` 进日常 CI |

内存单列：Vision 首次拉起 +20~40MB 实测，超标走 OCR 独立进程（socket 请求，主进程零加载）；Linux tray GTK loop 超标走 SNI。两条都是保 host 苗条，插件无感。

### 7.6 Wayland 降级契约（写死，不临时发挥）

无热键、无注入、无检测。可用形态：wl 监听 + 主弹窗（快捷键呼出）+ 复制模式（Enter=复制，不模拟）+ `clipx --qf/--fill/--copy` + 复制路径跳转。与 CopyQ 同水位，文档化，不追平。host 侧表现为整组 capability `tier()==Degraded/Unavailable`，插件代码零分支。

### 7.7 M6/M7 到 Steps 的映射

M6a（NSPanel spike）是 C1/C2 跨平台的生死门，插在 Step 2 之前做（spike 失败只影响窗口模型，不影响 Step 0/1 的路由与 trait）；M6b-e（采集/粘贴/OCR/跳转/打包）= 逐个填 §7.4 后端；M7a-c（X11/Wayland/对话框链）= 同一批后端的 Linux 分支。原则：**runtime 接口先行、后端随后，任何后端都不得要求改插件代码**——要求改，就说明接口错了，改接口。

## 8. 预留：AI 与同步（本次只留接口与政策，不实现）

- **AI 文本能力位**：`Action::Transform` 预留改写/翻译/摘要/语气，政策先行——秘密与密码框内容永不送模型（host 在 `Transform` 入口强制拒绝 `PayloadRef::Secret`）；模型后端（本地 ONNX 走 OCR-pack 的复用路线、远端 API 走用户自配 key）以后再定。2026 年的输入增强没有 AI 位等于没留门，但现在不做。
- **同步与身份位**：PRD v2 候选的 WebDAV/MQTT 只留接口——设备 ID、加密 blob  envelope（opaque，host 不解读）、冲突策略（last-write + pinned 豁免），插件数据经 `RtCaps::sync_blob` 读写。 vault 本身不同步（那是 Bitwarden 的事），同步的是历史/短语/收藏。
- **输入法共存**：TSF profile 教训（不重置输入法中/英态）上升为铁律——任何注入/焦点操作不得改变 IME 状态，回归用 `ime_state_test.py` 锁死；mac Secure Input/Win 密码框下增强全静音。

## 9. 生态与版本政策

- **内置优先**：v1 只内置插件，静态注册；第三方先用命令式（`commands: [{name, cmd, args, input, output}]`，stdout JSON 契约，约 100 行）——零 SDK、零 ABI、零常驻。
- **稳定性政策**：Step 4 前 trait 仅 workspace 可见；稳定后 semver：新增能力只加不改，删除走两版本 deprecation；JSON 配置只增字段；`HostCaps::api_version()` 供调用方自适应。
- **文档即 DX**：每能力一页（触发器/预算/降级/示例），`--probe` 即活文档；dogfood 顺序：自用 → 内置插件互调 → 命令式第三方 → 脚本/WASM（另议）。
- **命名**：二进制仍叫 clipx（runtime 即产品）；插件 id 扁平命名（`history/quickfind/filejump/inline/phrases/vault/commands`），保留 `ext.*` 前缀给未来第三方。

## 10. 工程计划（门控式，每步可发布、可回滚）

| 步 | 内容 | 量级 | 门（必须全过才进下一步） |
|---|---|---|---|
| Step 0 | `clipx-host` 门面（C1~C5 re-export）+ `KeySource`/`PlatformPaste` 骨架 + stub + CI 三平台 check + **P0 日志整改**（inline 明文击键、49 处审计）+ 设置快照 map 化 + §8 文档三处更新 | 3~4 天 | 全绿；快照一致；`inline_focus.log` 无原文；`cargo check` 三平台绿 |
| Step 1 | 迁 inline（最小插件，验 Manifest/Router/PluginEvt/崩溃隔离第一例） | 2 天 | inline 验收；`catch_unwind` 单测（插件 panic 不倒进程） |
| Step 2 | 迁 QF（验会话+代际+自有窗口）；`findx2 UDS` 传输分支顺手（mac/Linux 可用） | 3~4 天 | M4 脚本；`--qf-demo` 快照；热路径 12~33ms |
| Step 3 | 迁 FJ（watcher→RtCaps，注入→FillCaps）；CLI `--fill/--copy` 无头动作 | 4~5 天 | M5 验收；注入/COM 单测；Wayland 降级文案 |
| Step 4 | 主弹窗行级化（History/Phrases + `ItemId` + 融合层 + 统一入口前缀） | 3~4 天 | M1/M3 复跑 ±30%；排序快照 ≥15 例；快照矩阵一致 |
| Step 5 | VaultPlugin（`bw` 只读 + ephemeral + 填充三模）+ 审计落盘 | 另排期 | M2 验收；秘密零落盘审计（库/日志/转储三查） |
| 预留 | AI-Transform 政策实现、同步 envelope、静态加密 opt-in | v2 | 各自 ADR |

`rank_results` 下沉带走 Schwartzian 不许"顺手优化"；`bools` 下标债 Step 0 还；QF/FJ 事件接线 Step 0 不动只搬状态机。

## 11. 验收与度量（常驻）

预算表：热键→首帧 <150ms；同步查询 <5ms；`on_key` 单插件 <5ms；钩子回调零阻塞；图片懒加载；常驻 10~30MB（mac Vision 实测单列）；`clipx-host` 零新增常驻依赖。回归网：单测（确定性）+ 活体（`#[ignore]`）+ 快照矩阵 + M1/M3/M4/M5 脚本数据段 + `--probe` 探针。发版收尾清单加三项：快照矩阵全过、`audit.log` 无内容、`panic.log` 无秘密。

## 12. 风险

| 风险 | 缓解 |
|---|---|
| 接口过早固化 | Step 4 前内部可见；对外只承诺 JSON |
| Slint 壳复用漏水 | spec 只到定位/显隐/重绘/透明度 |
| `ItemId` 漏比较点 | grep 列全逐处改 + 单测；`preview_cache` key 换 `ItemId` |
| QF/FJ 迁移抖动 v0.10.12/13 链路 | 先不动接线；`--qf-demo` 两轮快照锁死 |
| mac NSPanel spike 失败 | 获焦退化，记 PRD §8；插件无感 |
| Vision 内存超标 | OCR 独立进程预案；host 苗条 |
| CLI 重写改坏调试入口 | `--dev-*` 行为不变对照表逐项验 |
| 日志整改漏网 | 49 处清单逐条打勾；`PayloadRef` 脱敏 `Debug` 编译期兜底 |
| 单人节奏 | 每步可停在日用态；随时可砍 Step 5 之后 |
