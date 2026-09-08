# clipx 里程碑路线图

> 状态：v1.7 · 2026-09-03 · 当前阶段：**对齐并超越 WPF 1.9.8**（面板高级交互 + FileJump/QF 收尾 + 来源/深搜/导出）。本机可关 `ClipboardX-filejump.exe`。

## 开发纪律

1. **单仓库推进**：所有工作在 clipboardx-rs 内完成，不再另起炉灶（历史教训：三次尝试三次重开，无一善终）。
2. **行为对齐 WPF 版**：交互行为以 WPF ClipboardX 为规格书，有意偏离需登记 PRD §8。
3. **里程碑必须可日用**：每个 M 结束时产品达到"愿意天天用"的状态，不达标不进下一个 M。
4. **core/UI 分离**：UI 框架相关代码不得进入 core/store/monitor/ocr（这是 ADR-001 中 egui 备案可切换的前提）。

## M0 骨架验证（预计 1-2 周）

目标：证明技术路线成立，跑通最小闭环。

范围：

- 初始化 workspace：删除空占位 build.rs，根 Cargo.toml 改写为 workspace，五个 crate 建立骨架并锁定依赖版本
- Windows 事件监听 → channel → core 去重 → SQLite 入库（含 FTS5 建表）
- Slint 最小弹窗：热键呼出、列表显示、Esc 关闭
- 完成 S1 / S2 / S3 三个 spike（见 ARCHITECTURE §8）

验收标准：

- 复制文本 1s 内出现在列表；**重启应用数据还在**（两个 tauri 原型从未跨过的门槛）
- 热键弹窗不抢原应用焦点（S1 通过）
- 2000 条文本滚动 ≥55fps，常驻内存 ≤30MB
- 连续复制 100 次无漏采、无重复风暴（S3 通过）

**M0 验收结果（2026-09-02，全部通过）**：

- 入库延迟 63-90ms（目标 <1s）；杀进程后 WAL 数据零丢失；重启 2300+ 条全部加载
- S1 通过：弹窗呼出不抢前台焦点；Esc 经 WH_KEYBOARD_LL 钩子关闭（见 ADR-009 前置与 §6）
- 常驻内存 19.7MB / 私有 4.2MB（软件渲染器，ADR-009）；100 连发后峰值 27.5MB，回落后 19.7MB
- S3 通过：100 连发 100/100 入库、0 重复；滚动 fps 留待手动验证（内存已达标，风险低）
- 验收脚本：`scripts/m0_acceptance.ps1`（冒烟/热键/连发/崩溃持久化）、`scripts/s1_focus.ps1`（焦点/Esc）

不在范围：搜索、图片、粘贴回写、托盘、设置界面。

## M1 Windows 日用（预计 2-3 周）

目标：替代手动 Ctrl+V 日常使用。

范围：键盘导航、拼音与文本搜索、粘贴回写 + ClipboardGate、系统托盘、设置持久化、单实例、失焦关闭。

日用形态：clipx（剪贴板）+ WPF ClipboardX-filejump.exe 双进程并行——FileJumpOnly flavor 同时包含 FileJump 与 Everything 快速查找（同属 CLIPX_FILEJUMP 门控，已从源码核实），过渡期功能零缺失。

验收：开发者本人连续一周将其作为主力剪贴板工具，期间发现的阻塞问题归零。

**M1 自动验收结果（2026-09-02，全部通过，脚本 `scripts/m1_acceptance.ps1`）**：

- 单实例互斥、Ctrl+Alt+V 呼出/隐藏、Esc/点击外部关闭均正常
- 拼音搜索：全拼 `shijie` 与首字母 `nh` 均命中「你好世界」并回车粘贴成功
- 数字快贴（空搜索下按数字直接粘贴对应行）、Enter 粘贴回写 + ClipboardGate 防自采、Delete 删除
- UI 复刻 WPF 版主题（#1E1E1E 系、accent #139493、圆角行、置顶 📌、空状态提示）
- 系统托盘（显示/退出）、设置持久化、失焦关闭

日用观察期与 M2 并行推进，期间发现的阻塞问题回流修复后再进 M3。

## M2 图片与 OCR（预计 2 周）

范围：图片采集、缩略图、原图懒加载、uniOCR 异步队列、OCR 文本入 FTS、S4 spike。

验收：

- 截图复制后缩略图即时可见
- OCR 完成后图中文字可搜
- 连续采集 100 张图片后，常驻内存回落至 ≤35MB
- 4K 大图预览不卡顿

**M2 自动验收结果（2026-09-03，全部通过，脚本 `scripts/m2_acceptance.ps1`）**：

- 启动常驻 23.1MB；截图复制即时入库 + 缩略图生成
- OCR（Windows Media OCR 直调，见 ADR-004 修订）：完成即回填 ocr_text，图中文字可搜——搜 "verify" 命中后 Enter 原样回贴 800×200 图片（DIB 回写），ClipboardGate 无自环
- Space 预览开/关正常；4K（3840×2160）预览解码限长边 1600，不崩不卡
- 100 连发 57.4s 全部入库（102/102 含前序 2 张）；空闲 trim 后工作集回落 0.2MB（≤35MB 达标，软缺页廉价拉回）
- 已知噪声：Arial 40pt 下 Media OCR 偶把 "l" 读作 "I"——验收脚本用无歧义子串断言，非产品缺陷
- schema 升至 v4：payloads 增 ocr_text 列，图片条数独立上限（max_image_items）

## M3 打磨与迁移（预计 2 周）

范围：文件列表与富文本格式、收藏、右键菜单、多屏定位、开机自启、WPF 数据迁移命令、万条压测。

验收：

- WPF 版全部历史迁移无丢失（含图片与 ocr_text）
- 万条历史下搜索 <100ms、滚动流畅
- 常驻内存稳定在 10-30MB 区间
- 核心路径体验对齐 WPF 版

**M3 验收结果（2026-09-03，脚本 `scripts/m3_acceptance.ps1`）**：

- 万条压测：seed 10000 条 162ms；空查询 0.92ms / FTS 英文 1.30ms / 中文子串 1.38ms / 编号子串 7.09ms（验收线 100ms，全部远低于线）
- WPF 真实库迁移：新增 6693 条、跳过重复 40；二跑零新增（content_hash 幂等）；text/files/images 三类唯一计数 WPF 与 clipx 完全一致（6174/369/150）；容量同步 max_items=20000（防迁移后首插即裁剪）
- 文件列表（CF_HDROP）与富文本（CF_UNICODETEXT + HTML Format 同场直写，模拟 Chrome/Word）采集均正确入库（kind=2/3）
- 置顶/删除/富文本回写数据层语义由 workspace 44 个单测覆盖（置顶排序+免裁剪、富文本回环+回写载荷、迁移幂等）
- 开机自启（schtasks XML）：注册/查询 Ready/删除全链路实测通过，无需管理员
- 常驻内存：OCR 回填排空 + 空闲 trim 后 WS 0.2MB
- **UI 段（F/G/H/J）待用户终端复跑**：TRAE 工具宿主进程窗口站权限被裁剪——SendInput/GetCursorPos/BitBlt 全部 err=5（EnumWindows/剪贴板正常），症状与锁屏一致但成因不同（详见 ARCHITECTURE §9）。须在用户自己的交互终端运行同一脚本取全量结果

排位检查点（确认型，非重开排序）：Windows 高级功能的排位已在规划期（2026-09-02）依据三项输入预先确定——FileJump 与 Everything 均为日均 10+ 次的高频使用、Mac 非日常开机（macOS 验证周期天然偏长）、跨平台 1.0 无发布硬节点——结论为 M4/M5（Everything、FileJump）先于 macOS 执行。本检查点只做三件事：复核 M0-M3 双进程共存的摩擦记录；从 Data/ 下 ShellNavigate 与 explorer_quickfind 两个日志提取实际使用分布，据此排定 M5 各文件管理器采集器的移植优先级；确认无新的相反证据。若共存摩擦在 M1/M2 期间提前激化，M4（Everything）依赖少、周期短，可提前插入。

## M4 Everything 集成（预计 1 周）

Windows 高级功能阶段一（PRD §7）。目标：Explorer 内快速查找回归，WPF 侧该功能下线。

范围：

- Everything 窗口消息 IPC（WM_COPYDATA）直连，替代 Everything64.dll SDK：查询协议封装，安装包不再分发原生 DLL
- ExplorerQuickFind 体验移植：活动资源管理器检测、快速查找弹窗（Slint）、结果导航与选中
- 移植基线（WPF EverythingIpc 源码注记）：搜索串用 parent: / path: 限定；勿依赖 SetMatchPath；「盘符:\ 关键词」形式在 IPC 实测恒 0 条

验收：Explorer 内热键呼出、输入即搜、回车跳转并选中，行为与性能对齐 WPF 版；Everything 未运行时的降级提示一致。

**M4 验收结果（2026-09-03，脚本 `scripts/m4_acceptance.ps1`）**：

- WM_COPYDATA 直连：官方 QUERYW 与 findx2 兼容布局自动协商；1.5 Alpha 窗口类后缀可识别
- session 0 服务：本会话无 IPC 窗口时 `-startup` 拉起用户态托盘客户端（不弹主窗口）
- 三阶段查询：`parent:` 一层 → `path:` 树下 → 全盘关键词；代际丢弃过期结果
- `parent:` 空结果 / Everything 不可达：当前文件夹 `read_dir` 兜底，浮层仍可用
- Explorer 检测：CabinetWClass 父链 + 桌面 Progman/WorkerW；编辑框/F2 重命名不触发
- 就地导航：Shell COM Navigate + SelectItem，失败回退 `SHOpenFolderAndSelectItems`
- 数据层单测覆盖表达式构造、结果合并/高亮、文件系统兜底；live 查询在 Everything 可达时断言 `parent:C:\Windows system32`
- **Explorer 内打字呼出**须用户终端手动点验（同 M3：工具宿主无法 SendInput）

## M5 FileJump 移植（预计 2-3 周，执行中）

Windows 高级功能阶段二，clipx 在 Windows 上补完最后一块。mac/Linux 无对等方案（见 ADR-008 修订：无 #32770/注入生态，不承诺移植，只做收藏+最近路径手动键入版，待 M5 后定）。

分步交付（每步独立可验收）：

- **M5a 对话框检测**：`clipx-filejump::dialog` —— #32770 类名 + 子控件特征（地址栏/文件名输入/Shell 视图，纯 Static+Button 消息框排除）+ WPS 套件识别（wps/et/wpp 进程 + 标题，Qt 空标题尺寸形态，WPS 内 #32770 消息框排除）+ IDMan 主界面排除。基线 `FileJump/FileDialogJumpHelper.cs`（WPF v1.9.8）。
- **M5b 路径采集**：`clipx-filejump::collectors` —— Explorer COM（`Shell.Application.Windows`）/ Total Commander 消息 / XYplorer WM_COPYDATA / DOpus dopusrt + 二档 UIA 白名单 + 收藏/最近/Z序。优先级按 M3 检查点日志分布：Explorer + TC 先行。基线 `FileJump/FileManagerPathCollector.cs`。
- **M5c 注入调度**：`clipx-filejump::inject` —— 复用 `../clipboard/native/ShellNavigate` 现成 DLL（不重写），宿主侧 `WM_USER+7 → IShellBrowser::BrowseObject` + 退避重试（`0/150/300/500/800/1200ms`，以 WPF 实测值为准，ROADMAP 旧值 0/120/300/600/1000/1500 已按源码纠偏）+ COM 借用指针禁 Release（硬约束）。WPS 永不注入，走 ValuePattern/ComboBoxEx/ReBar/Alt+D/Ctrl+L 六重降级。基线 `FileJump/ShellDialogDeepNavigate.cs`。
- **M5d Picker UI + 全局 Ctrl+G**：`ui/filejump.slint` + 托盘/热键接入 —— 贴框/跟鼠标、收藏⭐、Everything 文件夹补充、无框时开收藏/常用选后在 Explorer 打开。

范围：

- 新 crate `clipx-filejump`：Windows 实现在 `cfg(windows)` 后，非 Windows 编译为 stub（ADR-008：编译与否不影响其他平台）
- 注入 DLL 复用 native/ShellNavigate 现有产物，不重写

验收：

- 对 WPF v1.9.8 行为回归：系统对话框原生跳转、浏览器/微信保存对话框多次切换稳定、WPS 场景无误触、常用管理器路径采集正确
- Windows 单进程运行，整机常驻内存回到 10-30MB 目标区间
- WPF 版退役（仅保留历史数据迁移入口）

**M5 数据层验收结果（2026-09-03，脚本 `scripts/m5_acceptance.ps1`，数据层 PASS）：**

- 新 crate `clipx-filejump`：`dialog`（#32770 + 子控件特征 + WPS/IDMan 排除 + 标题启发式）、`collectors`（TC 1075/2029/2030、XY WM_COPYDATA、DOpus dopusrt XML、Explorer COM + Edit 回退）、`inject`（DLL 复用 + 退避 0/150/300/500/800/1200ms + 跨架构导出解析 + Alt+D/Ctrl+L 键盘链，WPS 永不注入）
- Picker UI（Slint `FileJumpWindow` 500px + 托盘入口 + 全局 `Ctrl+G` + 对话框前台 400ms 轮询自动弹出 + 自动跳转最佳 + Everything 文件夹补充 + 收藏/最近持久化）
- 单测 14/14 绿；workspace 全绿 80 passed；`--release check` 干净
- DLL 随包：`ClipboardXShellNavigate.dll` / `ClipboardXShellNavigate32.dll`（`../clipboard/native/ShellNavigate/bin` 产物）须与 `clipx.exe` 同目录，验收脚本 [C] 段自动 staging
- **UI 手动回归待用户终端**：记事本另存为框 Ctrl+G 跳转、前台自动弹出、无框全局模式、托盘项、WPS 无误触（工具宿主窗口站受限 + 日用实例占用 exe 锁，release 链接亦留终端执行）

## 对齐并超越 WPF 1.9.8（2026-09-03）

单进程吸收 FileJumpOnly：剪贴板面板 + FileJump + Explorer 打字查找同进程。安装包见 `scripts/clipx.iss`（便携 `Data/` 与 exe 同级）。托盘约 45s 静默查 GitHub Releases。

**本机手测清单（关 WPF FileJumpOnly 后）：**

- 剪贴板：呼出 / 搜索高亮 / 编辑文本 Ctrl+Enter / 钉住后粘贴不关 / Shift↑↓ 多选连贴 / Del 二次确认 / OCR 粘贴 / 作为文件粘贴 / Win+V 不闪开始菜单
- 批量：FIFO/LIFO 新复制入队、Alt 一次贴完、终端 Shift+Insert
- 设置：短语 CRUD、模拟粘贴、深搜、单图上限、FileJump 开关
- FileJump：记事本另存为 Ctrl+G、延时内二次 Ctrl+G 直跳、Tab 仅收藏、切回自动同步、托盘探测自定义对话框
- QuickFind：Explorer 打字；对话框前台 DirectOpen 导航，否则 ShellExecute；剪贴板仍显示时 Explorer 不吞键
- 超越：来源筛选、深搜、托盘导出导入、图片另存/复制路径
- 内存：常驻仍按 ≤30MB 本机复测（上次 M0 空闲 19.7MB）

## M6 macOS（预计 2-3 周）

范围：monitor 的 macOS 实现（changeCount 轮询）、Vision OCR、NSPanel 风格弹窗、LoginItems 自启、Cmd+V 模拟、打包（Developer ID 签名与 notarization 视开发者账号情况，无账号则先出未签名包）。

验收：Mac 上完成与 M1 等价的日用验收。

## M7 Linux 与发布（预计 3 周）

范围：X11 全功能、Wayland（wl-clipboard-rs + 热键方案评估）、GTK 托盘适配、Tesseract OCR、三平台 CI 矩阵与安装包（Windows Inno Setup / macOS dmg / Linux deb + AppImage，流程沿用 findx 的 CI 模式）、首个全平台 release。

验收：三平台 CI 绿灯；每平台过冒烟清单；GitHub Release 发布成功——首发即含完整 Windows 能力（剪贴板 + FileJump + Everything）。

## 风险跟踪

| 风险 | 影响 | 缓解 | 状态 |
|---|---|---|---|
| Slint 弹窗或列表不达标（S1/S2） | 路线返工 | 前置到 M0 首日验证；egui 备案条件已写死 | S1 已通过；S2 内存达标、fps 待手动验证 |
| Wayland 监听与热键 | M7 范围 | 业界普遍难点（CopyQ 在 Sway 亦有 issue）；最坏情况 Wayland 仅支持复制监听 | 接受 |
| uniOCR 中文质量（S4） | OCR 体验 | 平台原生引擎直调替换路径已定 | 待验证 |
| tray-icon 在 Linux 需 GTK loop | Linux 内存 | M7 实测；超标则直连 StatusNotifierItem | 待验证 |
| 单人开发节奏 | 周期 | 里程碑粒度小、每段可日用，随时可停在可用状态 | 纪律约束 |
| FileJump 移植的 Win32 脆弱面（注入/COM/各家管理器私有协议） | M5 | 复用已验证的原生 DLL 与 v1.9.7/1.9.8 行为基线；feature 门控隔离在 clipx-filejump | 数据层 + Picker 已落地，可关 WPF 双进程 |

## 里程碑之外的持续事项

- 每完成一个 M：更新 CHANGELOG、打 tag、发版（沿用 findx 的 CI 模式）
- 文档随代码同步：PRD / ARCHITECTURE / ROADMAP 的变更与代码同一提交，避免文档腐化
- 内存与性能实测数字随每个 M 更新到 PRD §5
- 欠账：WorkBuddy（Electron）热键弹窗相对输入框定位仍不稳（对话小窗会盖聊天、首页偶贴地），见 `crates/clipx-app/src/win_popup.rs` 的 `TODO(workbuddy-pos)`，回头单开；不要再对齐 WPF 定位
