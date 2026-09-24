# Changelog

本项目遵循里程碑发版（见 docs/ROADMAP.md），tag `v*` 触发 CI。

## v0.10.9 — 检索提速与批量粘贴可靠性（2026-09-24）

两处用户报障：**批量粘贴在 Cursor 的输入框里时好时坏**、**剪贴板搜索卡顿（首次尤甚）**。
五处根因都不在「用法」，而在移植时丢掉的细节、以及移植后顺手扩大的判定。

### 检索提速：拼音列从图片表里搬出来（本机 47ms → 3~5ms）

`payloads` 与 `image_blob` 同居（本机 111MB 库 / 7228 条，其中 image_blob 53MB）。检索里的
`p.pinyin_blob LIKE '%词%'` 是前导通配、用不上索引，必然全表扫 —— 每行还要跨溢出页取记录。
分段实测（同库同词）：

| 片段 | 耗时 |
|------|------|
| `entries.preview LIKE` | 3~10ms |
| `payloads.pinyin_blob LIKE`（JOIN payloads） | **73~95ms** |
| `entries_fts MATCH` | 0.5~3ms |
| 同一份数据放进只含两列的窄表 | **2~3ms** |

于是 `SCHEMA_VERSION` 7 → 8，新增窄表 `payload_search(entry_id, pinyin_blob)`，检索改走它；
`payloads` 上挂三个触发器（INSERT / `UPDATE OF pinyin_blob` / DELETE）自动同步，
**9 处写入点一行都没改**。深搜（`full_text` / `ocr_text`）仍回 `payloads`，只有 `deep = true` 才 JOIN。

- 真实库端到端（`migration_on_real_db` 自检）：迁移 + 打开 **63.8ms**、首次搜索 **7.3ms**、二次 **5.4ms**。
- 语义未变：同一组关键词优化前后的命中行数逐条一致（46/12/200/192/200）。
- 「首次特别卡」= 冷缓存下首次要读那 63MB 的 payloads；不再走它之后这段代价直接消失。

### 迁移不再重建 FTS（升级不再卡启动）

原迁移每次升级都 `DROP` + 重建 `entries_fts` 并逐行重算拼音 —— 那只有 v1/v2 → v3（加
`pinyin_blob` 列）那次需要。现改为按需执行：111MB 生产库上这是「秒级同步阻塞」与「63.8ms」的
差别，而它跑在 `Store::open` 的同步路径里 = 用户看到的是启动卡住。

### 修：批量粘贴在 Cursor 等 Electron 应用里「有概率粘不出内容」

文本写剪贴板改为**单次 `OpenClipboard` 周期内清空 + 写入**，失败做**真实等待**的重试
（20 × 15ms ≈ 300ms）。旧实现两处问题：

1. clipboard-rs 的 `clear()` + `set_text()` 是两次独立 `OpenClipboard` —— clear 成功而 set 失败时，
   剪贴板被留成**空的**；
2. 它的重试（clipboard-win `new_attempts(10)`）每次失败只 `Sleep(0)`：让出时间片、不等待，
   争抢下 10 次重试在微秒内跑完，**等于没有重试**。WPF 老版用的 WinForms `Clipboard.SetText`
   是 10 次 × 100ms 的真实等待 —— 同一个目标应用在老版贴得上、在 clipx 时好时坏，差异就在这。

Electron（Cursor / VS Code）的粘贴走异步 IPC，读剪贴板的时刻会落在我们「松开粘贴键即写回」
之后，两个进程 OpenClipboard 重叠的概率远高于原生应用。富文本（CF_HTML）保留 clipboard-rs
的头部生成，补上同样的真实重试。

### 修：批量推进的触发判定（「时好时坏」的另一半根因）

批量队列的推进靠监听目标应用里的 Ctrl+V / Shift+Insert **松键**。旧实现在**松键那一刻
现读物理键态**（`GetAsyncKeyState`）判断修饰键，两个问题：

1. **松键顺序**：用户把 Ctrl 比 V 先松开（连着快按时很常见，先后由硬件顺序决定）→ 那一次
   判定为「不是 Ctrl+V」→ **丢一次推进**：队列不动、剪贴板还是上一条。
2. **违反本文件既定策略**：`keyboard_hook.rs` 文件头写着「不信 `GetAsyncKeyState`，只信
   自记账位」—— 因为被钩子吞掉的键不进系统输入队列，物理键态会停在过期值（同一机制造成过
   「Alt 呼出热键匹配失败」的实测 bug）。而面板可见时 Alt 的按下/抬起正好会被吞。

改为**按下时武装、松开时只看武装位**：修饰键一律取自记账位（`CTRL_HELD`/`ALT_HELD`/`SHIFT_HELD`），
松键顺序不再影响结果。裸 `v`、`Shift+V`、`Ctrl+Shift+Insert` 都不武装 —— 在目标输入框里打字
不会误推进队列。新增纯函数单测 `paste_advance_arms_only_for_bare_paste_combos`。

### 修：Cursor / VS Code 不再被当成终端（收回 `9ae9c09` 的顺手扩大）

`is_terminal_process_name` 里有 `cursor` / `code` / `code - insiders`（`9ae9c09` 与一批 SSH
客户端一起加的，WPF 老版的 `PasteTargetHeuristics` **没有**）。判定只看「顶层窗口所属进程名」，
而 Electron 编辑器的集成终端是画在**主窗口**里的（没有独立 HWND）—— 于是整个编辑器 / 对话输入框
都被判成终端：用户配置的 Ctrl+V 被换成 Shift+Insert，文本还被去掉 CR。

而 VS Code 官方文档写明：**Windows 下集成终端的复制粘贴就是 Ctrl+C / Ctrl+V**（只有 Linux 是
Ctrl+Shift+V）。也就是说这三项既没必要也有害，属相对 WPF 的回归，本版收回（真实终端类
`ConsoleWindowClass`/`CASCADIA_*` 与 `cmd`/`pwsh`/`conhost`/`mintty`/`wezterm-gui` 等一律保留）。

> 取舍：收回后，在 Cursor 内嵌 WSL/Linux PTY 里贴多行文本不再自动去 CR（可能显示 `^M`）。
> 这是「无法从 HWND 区分编辑器与集成终端」的必然代价；真遇到再加按应用的强制终端规则。

### 可观测性：粘贴失败不再静默

单条粘贴失败 → 可见提示；批量推进失败 → `Data/batch_paste.log`。此前批量失败只回滚队列、
不留任何痕，用户只能凭体感描述「时好时坏」。

### 自检

- `CLIPX_MIGRATE_PROBE=<库副本> cargo test -p clipx-store migration_on_real_db -- --ignored`：
  真实库上的迁移 + 检索计时。
- `cargo test -p clipx-app write_text_survives_contention -- --ignored`：剪贴板被占用时写入仍成功。
  ⚠️ 必须在**能访问剪贴板的宿主机**上跑 —— 工具宿主沙箱内 `OpenClipboard` 直接 `ERROR_ACCESS_DENIED(5)`。
- 新增 `v7_to_v8_migration_builds_search_table_and_keeps_triggers` 迁移测试（含触发器同步）。
- 新增 `paste_advance_arms_only_for_bare_paste_combos`（批量推进的触发判定纯函数单测）与
  `terminal_class_and_process` 更新（Cursor / VS Code 不属于终端）。

### 修：非 Windows 的测试目标编译不过（CI 自 09-22 起红了三轮）

`legacy_wpf::log()` 是跨平台 `pub fn`，却无条件调用 Windows 专有的 `win_popup::write_data_log`
→ ubuntu / macOS 的 test job 报 `E0425: cannot find function write_data_log`。本机 `cargo check`
是 Windows target，看不见。同目录的 `append_debug_log` / `resolve_popup_anchor` / `fg_debug` 早有
`#[cfg(not(windows))]` 存根，这个当初漏了 —— 补上即可（`write_data_log` 落 `Data/` 只对
Windows 有意义，非 Windows 空实现符合语义）。

## v0.10.9 — 呼出定位三修 + Win+V 副作用（2026-09-22，同版本一并发布）

三处用户报障，一句话概括：**WorkBuddy 弹窗被今天新加的 caret2 带偏、Win+V 注入的
Escape 打到目标应用、开始菜单盖住弹窗**。

### 修：WorkBuddy 弹窗回退到「输入框上方居中」

`b94d7dc` 把 caret2 的插入框改成以光标为对称心的 ±380 假框，用户实测「更不好用了，
不如恢复输入框上方居中」。两处问题：

- **横向随光标抖**：假框中心 = 光标位置，再 `x = bl + (bw-pw)/2` 居中 → 弹窗整体
  跟着光标横移，视觉上「飘」。
- **跨屏错屏**（根因）：`bx0 = (pt.0-380).max(fg.0+8)` 用**前台窗左边界**（跨屏物理
  坐标）做 clamp，而 `popup_rect_on_box` 的 work 是按**锚点**取的屏。两者坐标系不
  一致时弹窗被甩到错误屏——`pos_debug.log` 实锤 `anchor=(2875,884)` 却算出
  `rect=(1976,0,904x1184)`（主屏右端，而 WorkBuddy 在副屏）。

改法：WorkBuddy 分支**不再走 caret2**（`uia_composer_box` 新增 `prefer_caret` 参数，
WorkBuddy 传 `false`），始终锚定 UIA 拿到的输入框 BBox（拿不到则几何估），弹窗水平
居中于输入框、垂直放其上/下——与 WPF `popup_rect_on_box` 语义一致。**跟光标走只保留
给未特判的应用**（微信仍走 caret/gui 路径）。

同时修通用跨屏 bug：`popup_rect` 的 `PLACE_ON_BOX` 分支改为**按输入框中心重新取
屏与 DPI**，不再沿用锚点屏。

### 修：Win+V 注入的 Escape 打到目标应用 + 开始菜单被收起

`intercept_win_v` 在拦截 Win+V 后、吞掉 Win KeyUp 时，会注入 `Escape + Escape抬 +
合成 Win抬`。两层问题：

1. Escape 本是「关掉可能闪出的开始菜单」，但**无条件发出**时会打到当前前台应用
   （WorkBuddy/微信等 Electron/Chromium 宿主）：裸 Escape 可能让输入框失焦、
   触发取消/关闭——用户报的「触发其他奇怪的快捷键」与「唤醒后可能丢焦点」。
   第一版改为**只在 Shell 前台时才补发 Escape**。
2. 用户复测又发现新问题：**开始菜单开着时按 Win+V 呼出剪贴板，松 Win 就把菜单收起来**。
   复盘定位到根因：**任何** Win keyup 到达系统（放行真实事件、或注入合成事件）都会被
   Shell 判为「Win 单按」→ **切换开始菜单**；而此前注入的 Escape 更是直接关菜单的键。

**最终方案**：Shell 前台（开始菜单/搜索正开着）时，**吞掉 Win KeyUp 且不注入任何键**
—— 系统收不到 Win up，菜单纹丝不动。非 Shell 前台才注入合成 Win KeyUp 重置「Win
卡住」状态。

**已知代价（登记 ROADMAP 遗留 #20）**：Shell 前台这条路吞了 Win up，系统会认为 Win
仍按着，**下次按 Win 可能需要按两下**。这是「保住菜单」与「Win 状态干净」的取舍；
若实测难以接受，改上「Win keydown 补配对」方案（复杂、需实测）。

注入动作**移到独立线程**：`SendInput` 前的 30ms 等待若留在低级键盘钩子回调里，
会卡住整个系统键盘（钩子 ~300ms 硬超时），对齐 `18924c5` 把钩子搬专用线程的教训。

日志 `Data/winv_debug.log` 每行带 `shell_open` 与最终动作（`swallow-no-inject` /
`swallow+inject`），便于真机对照。

### 加：开始菜单/搜索前台时固定定位 + 尽量插到 Shell 之上

开始菜单是无边框居中全屏浮层，Win11 还把它放在更高 Z 带，弹窗跟光标放必被盖。
移植 WPF 两条对策：

- `is_shell_foreground()`：认 `StartMenuExperienceHost` / `SearchHost` /
  `ShellExperienceHost` / `ShellHost`；`explorer.exe` 需再看类名（只认 WinUI
  `Windows.UI.Core.CoreWindow`，**不把 CabinetWClass 文件窗口误判成 Shell**）。
- Shell 前台 → `resolve_popup_anchor` 走 `shell-workarea` 分支：固定到当前显示器
  工作区左上 + 16px（WPF `PositionPopupFixedShellWorkArea`），重叠面积最小。
- `commit_hwnd_placement`：Shell 前台时先置 TOPMOST，再把本窗插到 Shell 根窗口之上
  （`SetWindowPos(insertAfter=GetAncestor(fg, GA_ROOT))`，对齐 WPF
  `ApplyShellForegroundZOrderFix`）——Win11 更高 Z 带下用户态压不过，属尽力而为。

### 加：自检注入口

- `--shell-demo`：强制「Shell 前台」形态（`set_shell_demo(true)`），呼出后
  `pos_debug.log` 应记 `branch=shell-workarea`，配 `--snapshot` 拍渲染。
- Win+V 拦截/注入全程写 `Data/winv_debug.log`（一次性低频动作恒留痕）。

## 未发版 — 老版 WPF 用户迁移收尾（2026-09-22）

### 加：检测老版 ClipboardX（安装 / 数据 / 运行 / 自启）

clipx 首启会自动导入老版历史（`wpf_import.rs`），但**老版程序本身还在跑**：
两套剪贴板监听并行、热键相撞、历史双写分叉。此前没有任何提示，用户不知道
「老版可以退休了」。

新模块 `legacy_wpf.rs` 做四路探测：

- **装过没**：按用户安装目录 `%LocalAppData%\Programs\ClipboardX` 探主程序
- **有数据没**：数据根 `%LocalAppData%\ClipboardX\clipboard_history.db`
- **在跑没**：`OpenMutexW` 探三个 flavor 的互斥体（`ClipboardX_F7A2E9B0` 等）
- **还自启没**：HKCU Run 值 `ClipboardX`/`ClipboardManager` + 登录计划任务
  `ClipboardX_AutoStart`（含 Dev 变体 `ClipboardX_AutoStart_Dev`）

探测放后台线程（读注册表 + 查计划任务 + 探互斥体，别卡启动），结论无条件写入
`Data/wpf_import.log`。**迁移是一次性动作，事后必须能查到「检测跑没跑、结论是什么」**。

### 加：「设置 → 关于」老版迁移卡片 + 一键停用自启

检测到「仍在自启」或「正在运行」时，关于页出现「老版 ClipboardX 迁移收尾」卡片，
提供「停用老版开机自启」按钮，并写明停用范围与卸载注意事项。

**停用只删自启项，绝不动程序目录与数据目录**——历史库要留着让用户确认迁移无误后
自行卸载。卡片里给全了关键警告：老版卸载向导选「是」会**递归删掉历史库**，
必须先确认导入、再卸载、且选「否」。

结果如实分项报告（不再只报「已完成」）：成功删掉的 Run 值与任务名逐条列出，
失败项带原文。老版管理员模式注册的任务是 `RunLevel=HighestAvailable`，
**普通权限删不掉**（`schtasks` 返回「拒绝访问」），此时给出可操作指引
（以管理员身份重启 clipx 后重试，或运行老版卸载程序）并**保留按钮**供重试，
不假装已解决。

### 修：`schtasks` 的中文错误信息变成一串 `?`

控制台程序输出的是 OEM 代码页（简中 Windows 上是 GBK）字节，此前用
`from_utf8_lossy` 直解，错误信息全废成 `??????`。新增 `decode_oem()`
（`MultiByteToWideChar` + `CP_OEMCP`）按系统代码页解码，并把「拒绝访问」
归一成「需管理员权限」。配套在 `clipx-app` 打开 `Win32_Globalization` feature。

### 加：迁移动作无条件落盘（不受 `CLIPX_DEBUG` 门控）

`append_debug_log` 未设环境变量时静默不写——迁移这类**一次性低频动作**必须恒留痕。
新增 `win_popup::write_data_log`（pub、无条件），`legacy_wpf::log` 走它。

## 未发版 — FIFO/LIFO 批量模式收尾（2026-09-22）

### 修：托盘图标丢掉了 F/L 字母，批量模式在托盘上分不出来

盘点批量模式现状时发现：clipx 从 WPF 移植 `TrayIconSvg` 时**只搬了配色、丢了字母**。

- WPF 原版（`clipboard/Media/TrayIconSvg.cs:45 CreateIcon`）在 FIFO 图标上叠一个 **"F"**、
  LIFO 叠 **"L"**，普通模式不叠 —— 而 clipx 的 `tray_glyph_for_mode` 只按亮度把
  `assets/tray.png` 重上色。青/蓝/琥珀三色在 16px 托盘尺寸下，**浅色任务栏上蓝与青几乎分不开**，
  字母才是最有效的那一档区分。
- 现按 WPF 规格补上：`tray_mode_palette` 给出「主色 + 浅条 + 字母」三元组，
  `draw_icon_letter` 用 5×7 点阵叠字（小尺寸下比矢量字体锐利），落点对齐 WPF 的
  `DrawString` 归一化坐标 **(0.583, 0.577)**。

### 修：批量胶囊不跟批次模式变色，与托盘不同步

底栏/表头的批量胶囊（`UiPill filled:true`）背景恒为 `Theme.accent`（青绿 `#139493`），
模式切到 FIFO/LIFO 时**托盘已变色、胶囊还是青绿**。现补 `Theme.batch-accent` /
`batch-accent-hover` 两个色位（由 `paint_theme` 按当前模式下发），`UiPill` 新增
`mode-tinted` 开关，批量胶囊置 true。其余胶囊保持青绿——它们跟的是品牌色，不是模式色。

### 加：`--batch-demo <off|fifo|lifo>` 自检开关

批次模式的可见差异分散在三处（托盘图标 F/L、批量胶囊配色、列表选中行配色），且都要求
「模式已切 + 队列非空」。不注入就只能连按热键手操，拍不稳也无法回归。
新开关一次把模式与队列（3 条）摆好，供 `--snapshot` 覆盖三种形态。

### 加：批次模式单测（此前为 0 覆盖）

`logic.rs` 测试模块原有 33 个用例，**batch 相关一个都没有**。现补 9 个纯逻辑用例
（三态循环、FIFO 尾插/LIFO 头插、去重移位、队首不变式、队首校验三重条件、
队列置顶排序及其跳过条件、胶囊文案、demo 注入），另加固有序/乱序队列的边界。
配套把入队/校验/排序抽成纯函数（`push_batch_queue` / `batch_head_ok` /
`reorder_queue_first_by` / `batch_label_for`），不必为测一个顺序去搭整个 `State`。

### 加：`dump_tray_icons` 手动自检（`#[ignore]`）

`cargo test -p clipx-app dump_tray_icons -- --ignored --nocapture` 把三种模式的托盘图标
落盘到 `target/tray-icon-{off,fifo,lifo}.png`。GUI 快照需要真实窗口站（工具宿主里跑不了），
这条走纯计算路径，无 GUI 依赖。生产与自检共用 `colorize_tray`，免得自检图与实际托盘图分叉。

### 更正：上一轮盘点里「Lifo 浅色 selected 饱和度偏弱」是错的

实测三模式浅色 selected 饱和度为 Off 0.26 / Fifo 0.34 / **Lifo 0.34**（与 Fifo 相同），
并非 Lifo 独弱；两两对比度 1.09–1.21 偏低是**三色共有的**、源于 WPF 规格的 10:15 混色
（本就大量向浅底色靠）。这属于规格如此，**不改** —— 改了就是无理由偏离 WPF。

## v0.10.8 — 更新中心重做（2026-09-21）

### 修：更新功能整体重做 —— 「能检测到新版本，但整个更新流程不可用」

用户报告的七条症状，逐条对应到真因（更新引擎 `update.rs` 的检查 / 下载 / 安装本身是完整的，
缺的是 **UI 状态机**）：

| 症状 | 真因 | 修法 |
|---|---|---|
| 提示「托盘右键 → 下载并安装更新」，但托盘根本没这一项 | 文案没跟 v0.10.6 托盘瘦身同步 | `UpdateAvailable` 文案改为指向「设置 → 关于」 |
| 「下载并安装」按钮恒存在，点了没反应 | 按钮静态无条件渲染，没有状态门控 | 加 `update-available` 状态，**仅在有新版时出现** |
| 提示是红框「错误」样式 | `set_notice` 把普通消息写进了 `error` 字段 | 拆成 `error`（红，校验失败）与 `notice`（中性，状态播报）两条独立展示位 |
| 找不到「自动检测更新 / 自动更新」开关 | 开关原在「常规」页，关于页没有 | 两个开关**集中到关于页「更新」区** |
| 看不到任何检查 / 下载进度 | 进度只走 2 秒自动收起的右下角提示条，不落设置窗 | 状态行 + 进度条落进关于页，`progress < 0` 走不确定态 |
| 无通知、无提醒 | 部分终态（已是最新 / 检查失败）静默返回 | 一律经 `push_update_state()` 回投状态区与提示条 |
| 改「启动时检查更新」开关不生效 | 检查只在 `main.rs` 启动时读一次 | 保存时记住改动前的值，「关→开」时立刻补跑一次 |

- **关于页（page 5）重做为「更新中心」**：状态行（一句话说清当前处于什么状态）+ 进度条 +
  「检查更新」（进行中变「更新中…」并禁用，避免并发重入）+「下载并安装」（仅新版时出现）+
  「下载页」/「项目主页」+ 两个开关 + 说明文字
- `UiBtn` 新增 `enabled` 属性（禁用态压暗为 0.45 透明度、不响应点击、不显示 hover）
- **事件通道分离**：`AppEvt::UpdateProgress` 增加 `done: bool` 显式标记终态（终态时按钮恢复
  可点、进度条收起）。OCR 拓展包下载原先蹭这条通道发，会被显示成「正在更新」并顺带禁用更新
  按钮 —— 改为独立的 `AppEvt::PackNotice { text, warn }`
- **`--settings-update-demo <state>` 自检开关**：注入关于页「更新」区形态
  （`idle` / `checking` / `downloading` / `downloading-nolen` / `available` / `failed`）。
  更新 UI 有 5 种形态，不注入就只能拍到 `idle` 那一种，其余分支要靠真实 GitHub 往返、
  拍不稳也无法回归
- 验证：`cargo check --workspace --all-targets` 0 警告；`--features ocr-rapid` 0 警告；
  `cargo test --workspace` 213 passed / 0 failed / 5 ignored；Light + Dark 双主题共 9 张
  快照覆盖全部形态

### 默认行为（用户确认）

- 「启动时自动检查更新」默认**开**，「发现新版本时自动下载并安装」默认**关**
  —— 即默认只在启动时查一次、发现新版后提示用户手动确认，**不静默下载安装**

## v0.10.7 — 快速查找 / 文件跳转浮层首帧残缺修复（2026-09-21）

### 修：常驻浮窗呼出瞬间「表头与底栏整块消失，过一会又自己好了」

- 症状（用户报告 + 截图）：Explorer 内打字呼出 Everything 快速查找时，**只有输入框渲染出来**，
  表头（`everything` / `N 项`）、1px 分隔线、底栏快捷键提示整块不见；等搜索结果回来把内容
  撑大后「自愈」。文件跳转（Ctrl+G）浮层同一路径，同样受影响
- 根因**不在 UI 代码**，在 Slint 软件渲染器的**增量重绘策略**：
  - `i-slint-backend-winit/renderer/sw.rs:111` 按 softbuffer 的 `buffer.age()` 选重绘策略，
    `age==1` → `RepaintBufferType::ReusedBuffer`，此时**只重绘脏区**
  - 常驻浮窗 `hide()`→`show()` 复用同一个 surface，而 softbuffer 的 Win32 后端
    （`softbuffer-0.4.8/src/backends/win32.rs`）`age()` 只看 `buffer.presented`、
    **不知道窗口被隐藏过**，且 `resize()` 遇相同尺寸直接 `return Ok(())` 不重建缓冲
  - 于是 `age()` 恒为 1，脏区外的元素永远不重绘；而表头标题、分隔线、底栏都是
    **无属性变化的静态元素**，一个都不在脏区里 → 整块留白
- 修法：**显示前把高度抖小 1px、显示后下一帧再恢复**，借两次尺寸变化强制 softbuffer
  重建缓冲（`age()`→0→`NewBuffer`，脏区=整窗）。顺序关键且**必须跨帧**——
  同帧抖动又改回则首帧看到的仍是原尺寸，等于没做
- 官方入口全部不可达（已源码级验证）：`force_screen_refresh()` / `mark_dirty_region()` 都够不到 ——
  `Window` 无 renderer 访问器，`WindowAdapter::renderer()` 返回**封印 trait** 无法向下转型，
  `i-slint-core` 又是 `slint` 的私有依赖。当前实现见 `win_popup.rs` 的
  `force_full_repaint_before_show` / `restore_size_after_show`
- 覆盖面：快速查找（`explorer_quickfind.rs`）与文件跳转（`filejump.rs`）两处常驻浮窗调用点

### 增：`--qf-demo <关键词>` 自检开关

- 跑**两轮**同一布局的会话（第二轮与第一轮窗口尺寸相同，正是复现路径），逐字输入后拍快照，
  产出 `out.png` / `out-r2.png` 与各一张 `-late.png`（结果到达、窗口长高那一帧）
- 验证记录：无命中（`shen`，440×240）、单命中（`深度`，440×240）、多命中（`x`，440×321）
  三种场景 + Light/Dark 双主题，表头、输入框、结果行、底栏全部首帧完整

### 文档

- `CLAUDE.md`「UI 开发陷阱」新增第 9 条：完整登记现象、根因（含 softbuffer 源码行号）、
  修法与「必须跨帧」的约束，并写明 **`--snapshot` 证明不了本 bug**（它绕过 softbuffer 的
  present 路径），须用 `PrintWindow(PW_RENDERFULLCONTENT)` 抓真实 HWND 像素验证
- `docs/ROADMAP.md`「遗留手动验证登记」新增 #13：真机点验快速查找 / 文件跳转呼出无残缺

## v0.10.6 — 反馈通道 + 托盘与设置重组（2026-09-21）

### 新：右下角提示条 —— 后台动作不再是「点了没反应」

- 症状：托盘右键里很多东西点了像没反应
- 根因**不是回调断线**。逐路径核过 Slint 1.17.1 的托盘实现（`WM_RBUTTONUP` → `TrackPopupMenu(TPM_RETURNCMD)`
  → `activate(cmd - 0x100)`），14 项回调全部正确接线。真问题在**反馈通道**：原来的 `notify()` 只写
  `state.notice` + 改托盘 tooltip + 刷新设置窗文本 —— 用户不开设置窗、不把鼠标悬到托盘图标上，
  就一个字都看不到。另有三处是裸 `return` 静默收场（自启/暂停无提示、跳转未启用直接返回、
  探针取不到前台窗口直接返回）
- 改法：新增 `ui/toast.slint` 常驻提示条（右下角，2 秒自动收起，可点击关闭），
  `notify()` / `notify_error()` 统一落到屏幕；上述三处静默 return 全部补反馈
- 提示条走 `win_popup::show_toast()`：定位只钉窗口位置（`SWP_NOSIZE`），**尺寸留给 Slint** ——
  首次实现把位置和尺寸一起按「逻辑 × scale」钉死，200% 缩放下窗口被砍半（400×108 → 200×54）

### 新：检查更新 / 自动安装有真进度条

- 原先下载阶段用 PowerShell `Invoke-WebRequest`，**整个文件下完前一个字节都不吐**，做不出进度
- 改为 `HttpClient` + `ResponseHeadersRead` 手工分块读，每块回报「已读/总量」，Rust 侧逐行解析；
  非 Windows 走 `curl --progress-bar` 解析百分比。下载 → 校验 → 安装三段都有文案与确定百分比
- 手动「检查更新」现在也回投结果（有新版 / 已是最新 / 失败），不再静默

### 新：设置窗口「关于」页

- 版本号、构建标识、数据目录路径，以及「检查更新 / 下载并安装 / 下载页 / 项目主页 / 打开数据目录」
- 托盘「关于 clipx」直接开在这一页

### 改：托盘菜单瘦身 14 项 → 6 项，腾出来的进设置「常规」页

- 托盘只留运行期高频入口：**显示 / 隐藏 · 暂停采集 · 清空历史 · 设置 · 关于 clipx · 退出**
- 其余 8 项按语义归位，一个都不少：
  - 开机自启 / 自动更新 → 新增的「常规」页「启动」区（开机自启本就在设置里有同一开关；
    **自动更新原先只有托盘这一个入口**，设置里根本没暴露，搬迁时必须补上，否则功能丢失）
  - 导出历史 / 导入历史 → 「常规」页「历史数据」区
  - 探测文件对话框 → 「常规」页「诊断」区
  - 检查更新 / 下载并安装更新 → 「关于」页（检查更新原本就在那儿）
  - 文件夹跳转 → 移除（低频功能，设置里有完整 tab，快捷键 `Ctrl+G` 照旧）
- 「剪贴板」页原先的「系统」区里，开机自启 / 管理员启动 / 启动检查更新 / 退出时清空历史
  四项搬去「常规」，剩下的改名「粘贴与识别」（替换 Win+V、合并粘贴、队列贴回、OCR 等）
- 设置 tab 顺序变为：剪贴板 0 · 常规 1 · 文件夹跳转 2 · 实验性 3 · 自定义对话框 4 · 关于 5
- 搬迁不丢反馈：开机自启原先由托盘路径 `notify_error` 报告注册失败，改走设置开关后会静默，
  现补在 `apply_settings` 里按「调用后 `is_enabled()` 是否等于期望」判定并回投设置窗红字
  （不取 `autostart::set` 返回值 —— 取消自启时 `schtasks /Delete` 对未注册任务也会返回失败，
  且 Linux 侧该方法恒定返回 false，拿它当判据会让保存设置永远失败）

### 改：设置六页按语义重排（「剪贴板」杂物间拆分）

- 原状：「剪贴板」一页塞 26 项混 8 类语义 ——「单图体积上限」（容量项）挂在「粘贴与识别」、
  「全文深搜」（检索项）也在粘贴区、「清空所有历史记录」埋在「快捷短语」末尾；
  「实验性」页装着早已稳定的「资源管理器快速查找」+「按键穿透」两个互不相干的功能；
  「自定义对话框」独立成页却只是跳转的补充配置，与它引用的探测按钮分居两页
- 新顺序：**剪贴板 0 · 记录与检索 1 · 常规 2 · 文件夹跳转 3 · 高级 4 · 关于 5**
  - 剪贴板 = 呼出与快捷键（**快捷键速查从「关于」页迁来**，与设置项对照）/ 面板外观 / 选择与粘贴
  - 记录与检索 = 容量（记录数、图片条数、**单图体积上限归位**）/ 检索（全文深搜）/ 图片文字识别 / 快捷短语
  - 常规 = 启动 / 更新 / 数据维护（导出、导入、**打开数据目录从「关于」页迁来**、退出清空）/ 危险操作（清空全部历史，两段确认，不再埋在短语区）
  - 文件夹跳转 = 跳转设置 + **「自定义对话框」整页并入** + 探测按钮（回调 `general-probe` → `fj-probe`）
  - 高级 = 原「实验性」改名，内容不动（两块功能早已稳定，旧名让人不敢碰、也搜不到）
  - 关于 = 版本 / 构建 / 更新与链接 / 数据目录（速查与打开目录按钮已迁走，不再与别页重复）
- 修一个**存量 bug**：`settings_win::request_procs_if_needed` 写死 `page != 2`，而调用点传
  `PAGE_EXPERIMENTAL = 3` —— 条件恒真，「排除应用」的「最近进程」列表自「常规」页插入后
  就从不加载。页码常量改 `pub(crate)` 单一真源（`crate::logic::PAGE_ADVANCED`），杜绝再错位
- 回调改名随迁：`about-open-data` → `general-open-data`（按钮搬去常规页）

### 验证

- `cargo check --workspace --all-targets` 与 `cargo check -p clipx-app --features ocr-rapid` 均 0 warning
- `cargo test --workspace`：**213 passed / 0 failed / 5 ignored**（ignored 为依赖本机状态的活体测试）
- 双主题快照过目：常规页（浅/深）、关于页（浅/深）、剪贴板页（浅）；6 个 tab 在 640px 宽下不挤
- 重排后逐页双主题快照 12 张过目（剪贴板 / 记录与检索 / 常规 / 文件夹跳转 / 高级 / 关于 × 浅 / 深）；
  关键项唯一性 grep 核对（清空历史、探测、导出导入等均只剩一处实体，其余为文字引用）

## v0.10.5 — 热键改键即时生效 + WPF 历史首启自动导入（2026-09-20）

### 修：改完快捷键不生效，必须重启程序

- 症状：默认 `` Ctrl+` `` 被别的程序占用时，在设置里换成任何快捷键都触发不了，只能重启 clipx
- 根因：热键线程**阻塞在 `GetMessageW`**。该线程只注册了 global-hotkey 的隐藏窗口，没有热键按下时一个消息都不来，于是循环体里的 `update_rx.try_recv()` 永不执行 —— 设置保存后的 `HotkeySet` 收不到、不重注册；而旧键本已注册失败、不会有 WM_HOTKEY 来唤醒，于是彻底锁死
- 改法：`MsgWaitForMultipleObjects(None, false, 50, QS_ALLINPUT)` 等「有消息或 50ms 超时」+ `PeekMessageW(PM_REMOVE)` 派发；有消息立即醒，无消息最多等 50ms（肉眼不可感）
- 配套：注册结果**回投 UI**（`AppEvt::HotkeyReport`）——设置窗口开着显示红字「快捷键 X 没能注册：多半已被其他程序占用，换一个再试」，未开窗走托盘提示；注册成功清空。原先失败只写 `eprintln` 与调试日志（还要 `CLIPX_DEBUG` 才落盘），用户完全看不到

### 新：WPF 版历史首启自动导入（装了就能看到老数据）

- 原先只有 `clipx --import-wpf <db>` 一条命令行，全新安装起来是空库
- 现在首次启动自动检测并导入，候选源库按优先级：`%LocalAppData%\ClipboardX\clipboard_history.db`（WPF 安装模式）→ clipx 同级 `Data/`（便携对便携）→ `../clipboard/Data/`（开发机）
- 后台线程分批进行，跑完写标记 `Data/.wpf-import.json`，此后不再自动触发；幂等（`content_hash` 去重），手动重跑仍是 `--import-wpf`
- 导入完成后托盘提示「已从 WPF 版导入 N 条历史」
- 读取改走新增的 `clipx_store::wpf::BatchReader`：rowid 游标分批，行数与图片 blob 字节双限（每批 ≤200 行且 ≤24MB），批间 20ms 让位给 UI 查询。源库实测 197MB / 7002 条（图片 blob 47MB），整读会顶穿 30MB 内存线，连批占满则会让 store 单线程通道排队、界面卡顿
- 容量跟随 WPF：源库同级 `settings.json` 的 `MaxItems`/`MaxImageItems` 更大则抬高（否则迁移完第一批新采集就按 clipx 默认容量把历史裁掉）；抬高后逻辑线程重载设置，保持 state 与磁盘一致

### 修：迁移丢了 WPF 已做过的 OCR

- WPF 的 `clipboard_history` 有 `ocr_text` 列（源库实测 146/150 张图有值），旧实现只读 7 列、没读它 —— 等于让用户把 OCR 重做一遍，且结果不一定能复现。ARCHITECTURE §4 的字段映射本来就写了要迁 `ocr_text`，是代码漏了
- 现补上，一并写 `payloads.pinyin_blob`、`entries_fts`、`entries.ocr_state = 2`（图片里的文字迁移后即可被全文/拼音搜到）

### 验证

- 真库端到端：**6962 条新增 / 40 条重复跳过 / 0 坏行**，26 秒（debug 版）；`ocr_text` 146 条一条不丢，FTS 与拼音索引均已生成，原时间戳保留；容量自动抬到 `max_items=20000 / max_image_items=150`
- `cargo test --workspace` 211 passed / 0 failed（新增 5 例：分批游标与坏行、字节预算不丢行、`ocr_text` 传递、`read_rows` 与分批读取一致、候选路径）
- 热键热更新与首启导入需真机点验，已登记 ROADMAP「遗留手动验证登记」#8 / #9

## v0.10.4 — 弹窗质感 + 检索体验 + 键盘翻译修正（2026-09-20）

### 弹窗质感（对齐并超越 WPF）

- 卡片底色改 `card-bg`（比 `popup-bg` 多约 5% 透明，对齐 WPF `PopupBgBrush`）
- 修三窗口卡片四角为直角：根因不是 `border-radius` 失效，而是软件渲染器 `combine_clip` 忽略 radius，贴边的不透明子元素以矩形填平四角。按 WPF 写法改（表头/空态铺满层透明，底栏与预览面板自带分角圆角）
- 5 处 emoji 补 `color`：软渲无彩色字形，不设即默认黑，暗色主题下整块隐形；空态新增 `UiEmptyIcon`（64px 圆底衬，📭 原先连 color 都没设）
- 列表 hover/selected 跟随批次模式主色（对齐 WPF `ApplyBatchModeChromeResources`）
- 新增 `UiScrollBar`（三窗口共用）：本渲染器下 ListView 不画滚动条，长列表只能盲滚。细滑块常态 4px / hover 撑到 6px，可点击跳转、可拖动，拖动中转 accent
- 新增 `UiCardSheen`：卡片上沿 1px 内高光（渐变与圆角在本渲染器下不可共存，只能用纯色实现）
- 过渡动画：列表行 110ms、按钮/胶囊/图标按钮 120ms（含 border-color）、开关 140ms；菜单浮层手工投影（`drop-shadow-*` 在软渲下是空实现）
- 三窗口表头内缩 12 → 16（对齐 WPF Margin 水平 16）；弹窗搜索栏补占位文案、放大镜转 accent；底栏 more 按钮补 hover 反馈

### 检索体验

- **空格 = 分词交集**（有意超越 WPF——WPF 版 Space 只切预览、搜不了空格）：空查询时仍是「切换预览」，一旦进入检索态就变成分词符，前后 token 取交集，底栏提示跟着状态改写「Space 预览 / Space 分词」。DB 侧同步改：原先整个 query（含空格）塞进一个 `LIKE '%a b%'`，带空格的查询**必然 0 结果**；现按空白分词，每 token 生成 `(preview LIKE ?n OR pinyin_blob LIKE ?n [OR full_text/OCR])` 再用 AND 连起来，FTS 分支保持整串 AND 前缀语义作为并集里另一条召回通道。旧的四分支 `match (fts, src)` 撑不住可变参数，改用 `Vec<rusqlite::types::Value>` + `params_from_iter` 动态编号占位符
- **拼音命中高亮**：此前只按字面找高亮、DB 却按 `pinyin_blob` 命中，于是「输入 pingjie 搜到『萍姐』却是白的」——检索与高亮两套判定不对齐。`split_hit` 加拼音回退，与检索侧 `text_matches_query` 的 AND 分词同构（逐 token 先字面、再拼音/首字母，各段区间取包络）；`clipx-core::pinyin` 新增 `pinyin_hit_spans`（全部命中区间，相邻合并），`consume_pinyin` 不再对汉字 token 提前 bail
- **不完全拼音也能高亮**：`pinyin_blob` 是全拼连写 + 首字母连写，`pin`/`pingj`/`ingj` 这类不完全音节本就能检索到；高亮侧原先逐字消费拼音（只认完整音节或音节前缀，碰到「，」这类无拼音字符就断），实测这三条旧实现全返回 None，于是「搜到了、一个字也不亮」。现改为按 blob 建「blob 字节区间 → 源字符下标」映射，直接在 blob 上找子串再映射回字符区间（`indexed_blob` / `map_byte_range`），与检索判定完全同构。部分音节 → 整字高亮；跨过中间非汉字（`jiew` = 姐+我）→ 连标点一起包进包络
- 命中色新增 `highlight` / `highlight-on-fill`（Nord aurora 黄）：选中与悬停行的底是主色混出的青，原 accent(#139493) 青字压青底会整段融进去。透明底 12.2:1(暗)/5.3:1(浅)、有色底 6.7:1/5.7:1
- `rank_score` 补拼音分档（起始命中 18 > 中间命中 8），否则拼音命中的整批结果全停在 0 分，只剩时间衰减撑着——高亮对了、位置却是乱的
- 有查询却一行都高亮不出来 ⇒ sub 追加「正文命中」：说明命中的是全文/OCR 而非 preview，不然这行看着就是误报
- QuickFind 同步按行状态切高亮色

### 输入修正

- **Shift+数字行整体错位**（Shift+1 出 `@`、Shift+2 出 `#`……每个键都拿到右邻键的上档字符）：根因是 `char_from_vk` 手写的 US 布局表，数字行写成 `"!@#$%^&*()".as_bytes()[(vk - 0x30)]`，而上档表按 vk 顺序应是 `)!@#$%^&*(`——整表右移一位。改为对齐 WPF `LowLevelKeyboardText.VkToChar` 的做法，直接用 `ToUnicodeEx(vk, scanCode, keyState, …, GetKeyboardLayout(0))`：左右 Shift 的 `0x10`/`0xA0` 都要置位、返回负数的死键必须再调一次冲掉、`translate`/`char_for_qf` 一并透传 scanCode；数字仍走 `KeyEvt::Digit`（快贴/筛选取值依赖它），只有上档符号走翻译。非 US 布局下手写表更是整片错乱，这层自造表本就不该有

### 自检设施

- 新增 `--query <text>`：与 `--uitest`/`--snapshot` 叠加，逐字走真实 `KeyEvt::Char` 通道输入（遇空格发 `KeyEvt::Space`），让快照能拍到搜索态与高亮（此前只能拍空搜索框，高亮改完无从验证）
- CLAUDE.md：「UI 开发陷阱」新增第 5、6 条（渐变不读 radius / 组件根元素不能用 parent）+ `--query` 用法 + `highlight_check.py` 像素判读法（抗锯齿会把字缘混向底色，但字身必有一批精确等于下发色值的像素，扫 d==0 即可）
- ROADMAP 新增「收尾检查清单（每次发版前逐条过）」：把 v0.10.3 收尾踩到的坑固化（警告看全量、测试确定性、版本三处同步、CHANGELOG 先补旧段再开新段、文档状态头、release 三产物校验、tag 后双层 CI 确认）
- 新增 `.workbuddy/skills/win32-key-to-char` 技能：把 `ToUnicodeEx` 翻译法（含死键、CapsLock、左右 Shift）与 `vk_probe.py` 对照脚本留作复用

### 测试

- 205 passed / 0 failed / 5 ignored（新增 4 例：多 token 区间、`indexed_blob`/`to_pinyin_blob` 逐字节一致的不变量、`split_hit` 拼音回退、空格交集 `search_spaces_are_and_tokens`）
- 注：`clipx-monitor::snapshot_classifies_text_plus_html_as_rich` 在本机因**系统剪贴板被外部程序独占**而失败（`OpenClipboard` 报 err=5，Python 独立调用同样失败，与本次改动无关）

## v0.10.3 — 图上 OCR 选词 + 高精度拓展包 + 文档预览（2026-09-20）

### 预览 OCR：微信/PixPin 式图上选词 + 高精度拓展包

- 图上选词：原图干净展示，拖选/已选命中的词直接在原字上变蓝；字上按下=选词，空白处按下=框选（1x）/平移（放大时）；松开定选区（单击取框/点空清选区），选区留存，右键/复制条/Ctrl+C 复制（弹窗选区保留）；无橡皮筋；双击全文；`Ctrl+C` 钩子上报不吞（TextInput 原生复制继续）
- 预览正文真拖选：只读 TextInput（原生拖选/Ctrl+C/选区高亮）；聚焦时钩子只留 Esc/Enter
- OCR 拓展包（`--features ocr-rapid`，默认不集成）：RapidOCR ONNX PP-OCRv6 small，`AutoOcrEngine` 按任务调度（session 用完即弃），失败回退 Media；`Data/ocr-models/` 首启自动下载（SHA256 校验）；设置 `ocr_engine` 自动/系统/拓展包（切换需重启）；macOS Vision / Linux Tesseract 原计划不变
- 文档型文件预览（新 crate `clipx-doc`，ADR-010）：文本摘录（UTF-8/BOM/UTF-16LE/GBK 回退）/文件夹清单/元数据卡；表格首表转文本（`calamine`）；docx/pptx 手解文本；PDF 提文本；右键打开/定位（复用 `clipx-jump`）
- schema v7：`payloads.ocr_boxes`（OCR 行/词框 JSON，旧行回填收敛）
- 文本预览修：纯文本不再占位空图盒；底部提示按类型区分；`char-wrap`；"剪贴板"错别字

### 收尾

- 清掉 6 条 dead_code 警告：`win_popup.rs` 三个早被 `resolve_popup_anchor`/`position_at` 取代的定位函数（含一条只断言死函数的僵尸测试）删除；`update.rs::InstallOutcome::ManualOpen` 加平台说明与 `allow`
- `clipx-everything` 两条活体测试（`live_roundtrip_if_everything_running` / `live_parent_scoped_query`）改为 `#[ignore]` + 文档注明手动命令：它们依赖本用户会话内 Everything 在跑，否则会真的拉起 `Everything.exe -startup` 并回包超时，使 `cargo test` 非确定。默认测试集自此确定性全绿

## v0.10.2 — 自动更新（检查 → 下载 → 安装 → 重启）（2026-09-10）

- Windows 一键自动更新：GitHub Releases 检查（**修正仓库地址** `chaojimct/clipx`，此前写错导致从未查到更新）→ 下载 `clipx-<v>-setup.exe` → Inno 静默安装（免 UAC，装到 `%LocalAppData%\clipx`）→ **自动重启新版本**
- macOS：下载 dmg 并打开（拖入 Applications）；Linux：deb 交软件中心 / 便携 tar.gz
- 托盘菜单新增「下载并安装更新」与「自动更新：开/关」（默认关；开启后启动检查发现新版即自动装）
- 便携模式（Data/ 与 exe 同级）不自动更新（避免数据目录分裂），提示手动下载
- 无 HTTP 依赖：复用系统工具（Windows PowerShell / macOS+Linux curl），与既有静默检查一致

## v0.10.1 — 对齐 WPF 1.9.8 + 跨平台地基（2026-09-10）

首个公开发版：Windows 全功能日用（对齐并超越 WPF 1.9.8），macOS/Linux 完成编译地基与平台代码（M6b 预置）。

对齐并超越 WPF 1.9.8：面板高级交互、批量补完、短语/设置接线、FileJump/QF 收尾、来源/深搜/导出。本机可关 `ClipboardX-filejump.exe`。

- 编辑文本（保留 id）、钉住弹窗、标题栏齿轮、Shift 多选连贴、Del 二次确认、OCR 粘贴、作为文件/JSON 粘贴、搜索命中高亮、Win+V 钩子注入 Win KeyUp
- 批量：相邻文本/图/文件合并、FIFO/LIFO 新复制入队、Alt 一次贴完、终端启发式 Shift+Insert；托盘图标随 FIFO/LIFO 变色
- 设置：短语 CRUD（触发词+正文）、`max_image_bytes`、模拟粘贴/FileJump/深搜开关；常用路径确认 N 次进收藏
- FileJump：`auto_sync` 切回刷新并跳外部目录、Tab 仅收藏、延时内二次 Ctrl+G 直跳、文件名框按键穿透、托盘探测自定义对话框；QF DirectOpen；Explorer 前台不吞键
- 超越：采集记来源应用、全文深搜、JSON 导出导入、图片另存/复制路径、相关度排序
- 托盘关于/检查更新（启动约 45s 静默查 GitHub Releases）；安装脚本 `scripts/clipx.iss`
- schema v6：`entries.source_app`

### 跨平台地基（2026-09-10，方案见 docs/CROSSPLATFORM.md）

- **M6b mac 平台实现**（macos runner 原生编译+测试全绿，真机行为待 dmg 验证）：`clipx-monitor::platform_macos`（clipboard-rs changeCount 轮询，采集次序对齐 Windows：文件/富文本/纯文本/图片）；paste CGEvent 合成 Cmd+V（core-graphics，需辅助功能权限）；autostart LaunchAgents plist 写入/卸载（未签名 bundle 适用；后续可升级 SMAppService）；OCR mac 为 M6c 独立步骤（Vision/objc2 接 OcrEngine trait）
- CI：三平台 `cargo test` 矩阵 + 纯 Rust 跨平台 check 防火墙 + **mac dmg 打包 job**（未签名 artifact，每次构建可下载，供真机 M6a 验证循环）
- 新 crate `clipx-jump`：跨平台「结果跳转」执行层——reveal（文件管理器中定位：Windows SHOpenFolderAndSelectItems / mac `open -R` / Linux `nautilus --select`→`dolphin --select`→`xdg-open` 回退链）与 open_path（ShellExecuteW / `open` / `xdg-open`）；Windows 真实弹窗冒烟通过
- `clipx-app` 的 `windows` 依赖移入 `[target.'cfg(windows)'.dependencies]`（跨平台编译第一道坎）
- `clipx-core` blake3 改 `pure` 纯 Rust 实现（默认 C SIMD 走 cc，交叉编译不可用；输出一致）
- `clipx-everything` 的 findx 客户端三平台化：Windows 命名管道 / macOS+Linux UDS（路径规则与 findx2-ipc 一致），`query`/`warmup` 统一走 findx 端点，Everything IPC 降级为 Windows 无 findx 回退
- 审计确认 app 层平台防护已就绪（`explorer_shell` `#![cfg(windows)]`、`keyboard_hook`/`mouse_hook` 主体在 `platform` mod、`win_popup` cfg use）
- 新增 `.github/workflows/ci.yml`：三平台 `cargo test` 矩阵 + 纯 Rust crate 跨平台 `cargo check` 防火墙

### 预览性能与检索拼音（2026-09-09/10）

- 预览三级渐进：64px 缩略图 → 1280 JPEG 渲染图（`preview_rendition` 磁盘缓存 worker，400 个/200MB 上限）→ 长边 1600 全解；WIC 边解边缩（`wic.rs`）替代全解再缩；预览滚轮缩放 + 拖拽平移 + "加载中…" 占位
- QuickFind 优先走 FindX 命名管道（JSON 行 + `pinyin: true`，搜得到中文名），单次关键词查询分本地/全盘两段；FindX 不可用回退 Everything 三阶段；`pinyin_hit_span` 拼音命中高亮
- 缩略图补全：文件列表首图缩略图（≤32MB 图片文件）+ 历史条目启动回填（`backfill_file_thumbs`）
- 粘贴图片 DIB+PNG 双写（兼容老应用）；热键集中注册（`set_app_hotkeys`）；`should_auto_elevate` 防重复提权；Explorer XAML 搜索框/重命名不触发 QF；远程桌面连接框排除出文件对话框识别

### 设置窗口重建与 WPF 图标（2026-09-10）

- 图标统一用 WPF 版 `assets/clipboard.ico`：exe/任务栏经 `app.rc` + Windows SDK rc.exe 嵌入；托盘 64px PNG 染色改 WPF 同档双色（主色+横条浅色，FIFO 蓝/LIFO 金）
- 设置窗口改为**每次打开销毁重建**：Slint 软件渲染器（ReusedBuffer）在常驻窗 hide→show 后只局部重绘，页面残缺/空白（切页才恢复）；新实例等价首次显示，根治。实例强引用只存事件循环线程（thread_local），新 weak 经 `AppEvt::SettingsWindowReady` 回传
- 设置窗口打开期间 `always-on-top`（Slint 原生），呼出后两轮尽力抢前台（AttachThreadInput + Alt 键 hack），拿到焦点即降回普通 z 序；标题栏/任务栏有图标
- 排雷：Slint 的 `raw_window_handle` 在本工程下报 "cannot be represented" 拿不到 HWND——`win_popup::find_window_by_title`（EnumWindows）替代；FileJump `FJ_HWND` 曾恒 0（dock 跟随/重停靠失效）同法修复

## v0.6.0-m5 — FileJump 文件夹跳转（2026-09-03，数据层）

M5a-d 数据层完成，UI 手动回归待用户终端点验。

- 新 crate `clipx-filejump`：对话框检测（#32770 + 子控件特征 + WPS/IDMan 排除）/ 路径采集（TC / XY / DOpus / Explorer COM + Edit 回退）/ 注入调度（复用 ShellNavigate DLL，退避重试，WPS 永不注入，Alt+D/Ctrl+L 键盘链）
- Picker：Slint 浮层 + 全局 Ctrl+G + 托盘入口 + 前台轮询自动弹出 + 自动跳转最佳 + Everything 文件夹补充 + 收藏/最近持久化（settings.json 新增 `filejump_*` 9 项）
- 验收：`scripts/m5_acceptance.ps1` 数据层 PASS；单测 14/14；workspace 80 passed
- 运行要求：两个 ShellNavigate DLL 须与 clipx.exe 同目录（验收脚本自动 staging）

## v0.5.0-m4 — Everything 快速查找（2026-09-03）

M4 完成：Explorer 内 Everything 快速查找回归，WPF 侧该功能可下线。

- 新 crate `clipx-everything`：WM_COPYDATA 直连，不分发 Everything64.dll
- 双布局协商：官方 QUERYW（x64 指针宽）与 findx2-service 兼容包；空串探测缓存
- Everything 1.5 Alpha 窗口类后缀；服务在 session 0 时 `-startup` 唤醒用户态客户端
- Explorer 打字会话：钩子快路径 <1ms，三阶段查询（parent: / path: / 全盘），↑↓/翻页/Ctrl+1-9/Enter 定位选中
- Everything 不可达或 parent: 空：当前文件夹文件系统兜底
- 验收：`scripts/m4_acceptance.ps1`；Explorer 内打字须用户终端手动点验

## v0.4.0-m3 — 打磨与迁移（2026-09-03）

M3 完成：格式补全 + 收藏 + 菜单 + 自启 + WPF 历史迁移。

- 格式补全：文件列表（CF_HDROP）与富文本（CF_UNICODETEXT + HTML Format，kind=2/3）；富文本粘贴回写投影 + HTML 双格式，html 缺失退化为纯文本
- 剪贴板原子快照（ARCHITECTURE §7.7）：类型判定与数据读取同一次 OpenClipboard 周期，open 争抢/中途被打断整体重试，杜绝富文本误判降级
- 收藏/置顶：Ctrl+P 或右键菜单切换，置顶浮动顶部（ORDER BY pinned DESC）且不受容量裁剪影响；选中跟随原条目
- 右键上下文菜单：复制到剪贴板（不模拟粘贴）/ 置顶 / 删除；Menu 键打开，Esc 只关菜单不关弹窗
- 多屏定位：MonitorFromPoint 光标所在显示器工作区夹紧
- 开机自启：schtasks XML（对齐 WPF v1.9.8：无执行时限、电池可用、仅当前用户登录触发），托盘菜单可切换
- WPF 迁移命令 `--import-wpf`：真实库 6693 条幂等迁移（content_hash 去重、保留时间戳与 ocr_text），容量设置自动跟随
- 万条压测 `--bench`：seed 10000 条 162ms，最慢查询路径 7.09ms（验收线 100ms）
- `--uitest` 参数：程序化显示弹窗（UI 截图验收用，绕过热键依赖）
- 验收：scripts/m3_acceptance.ps1 剪贴板段全部通过；UI 段（F/G/H/J）待用户交互终端复跑（工具宿主环境窗口站权限受限，ARCHITECTURE §9）；单测 44/44 绿

## v0.3.0-m2 — 图片与 OCR（2026-09-03）

M2 完成：图片全链路 + OCR 可搜。

- 图片采集：位图 → PNG 入库（15MB 上限），宽 64px 等比缩略图即时生成
- OCR：Windows Media OCR 直调（windows crate WinRT，ADR-004 修订），有界队列异步执行、即用即释；CJK 后处理移植 WPF 版 OcrTextPostProcessor
- OCR 文本可搜：搜图中文字命中后 Enter 原样回贴图片（DIB 回写）
- Space 预览：原图懒加载（解码限长边 1600）+ OCR 文本展示，4K 图不崩不卡
- schema v4：payloads 增 ocr_text 列；总条数与图片条数双轨容量裁剪（max_items / max_image_items）
- 内存：启动 23.1MB；100 图连发峰值 82.8MB，空闲 trim 后回落 0.2MB
- 验收：scripts/m2_acceptance.ps1 全部通过；单测 37/37 绿

## v0.2.0-m1 — Windows 日用（2026-09-02）

M1 完成：可作为主力剪贴板工具日用。

- 搜索：FTS5 英文/数字词匹配 + LIKE 子串（preview / full_text）+ 拼音 blob（全拼连写 + 首字母连写，`nihao`/`nh` 均可命中）
- 键盘导航：↑↓ 移动、Enter 粘贴、Esc 关闭、可打印字符直接进搜索、空搜索下数字 1-9 快贴
- 粘贴回写：ClipboardGate 防自采 + Ctrl+V 模拟（不抢前台焦点）
- 无焦点输入：WH_KEYBOARD_LL 钩子接收弹窗按键（含 Shift 符号、小键盘数字），WH_MOUSE_LL 点击外部关闭
- UI 复刻 WPF 版：#1E1E1E 暗色主题、accent #139493、圆角行选中态、置顶 📌 徽标、空状态提示
- 系统托盘（显示/退出）、设置持久化（settings.json）、单实例互斥、失焦关闭
- store schema v3：payloads 表 + pinyin_blob 列，v1/v2 自动迁移
- 验收：scripts/m1_acceptance.ps1 全部通过；单测 8/8 绿

## v0.1.0-m0 — 骨架验证（2026-09-02）

M0 完成：技术路线跑通最小闭环。

- workspace 五 crate 骨架（core / store / monitor / ocr / app），依赖版本锁定
- Windows 剪贴板事件监听 → channel → blake3 去重 → SQLite（WAL + FTS5）入库
- Slint 最小弹窗：Ctrl+Alt+V 呼出/隐藏、列表显示、Esc 关闭（WH_KEYBOARD_LL 钩子，不抢前台焦点）
- 软件渲染器（ADR-009）：常驻 19.7MB / 私有 4.2MB，达标 10-30MB 目标
- 验收：入库延迟 63ms；100 连发 100/100 零漏采零重复；杀进程 WAL 零丢失；重启数据完整
- 验收脚本：scripts/m0_acceptance.ps1、scripts/s1_focus.ps1
