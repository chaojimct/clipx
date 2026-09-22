# clipx 里程碑路线图

> 状态：v1.9 · 2026-09-21 · 当前阶段：**v0.10.8 已收尾，Windows 全功能日用**（对齐并超越 WPF 1.9.8 + WPF 历史首启自动导入 + 热键改键即时生效 + 弹窗质感 + 检索体验 + 图上 OCR 选词 + 文档预览 + 自动更新 + 右下角提示条反馈 + 更新下载进度 + 托盘瘦身与设置六页重排 + 浮层首帧残缺修复 + 更新中心重做）。下一个迭代进入 **M6 macOS**。本机可关 `ClipboardX-filejump.exe`。
>
> 遗留手动验证项集中在 [§遗留手动验证登记](#遗留手动验证登记2026-09-20) —— 新迭代开工前先看那一节。

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

## v0.10.x 能力增量（2026-09-10 / 09-20 / 09-21）

六版均为 Windows 日用形态叠加，无 scope 扩张；跨平台主线（M6/M7）未动。

- **v0.10.1** 对齐并超越 WPF 1.9.8 + 跨平台地基：三平台 CI 矩阵、mac dmg 打包 job、新 crate `clipx-jump`（reveal/open_path）、blake3 改 pure 纯 Rust、findx 客户端三平台化
- **v0.10.2** 应用内自动更新：启动静默查 GitHub Releases → 下载 → Inno 静默安装 → 自动重启；托盘开关（默认关），便携模式不自动更（避免数据目录分裂）
- **v0.10.3** 图上 OCR 选词 + 精度拓展包 + 文档预览：行/词框入库（schema v7 `payloads.ocr_boxes`）→ 预览图上叠加可框选文本层；RapidOCR ONNX 拓展包（feature `ocr-rapid` 默认关，缺模型首启自动下载）；新 crate `clipx-doc` 文档型文件预览。见 ADR-010 / 011 / 012
- **v0.10.4** 弹窗质感 + 检索体验 + 键盘翻译（无 schema 变更）：自绘滚动条 / 卡片内高光 / 过渡动画 / 暗色 emoji 修复（四角直角真因是软渲 `combine_clip` 忽略 radius）；拼音命中高亮与检索判定同构（`indexed_blob` 字节区间↔字符映射），不完全拼音（`pin`/`pingj`）可高亮，检索态空格为分词交集（**有意超越 WPF**——WPF 版 Space 只切预览）；`char_from_vk` 手写 US 布局表整表右移 → 改 `ToUnicodeEx`
- **v0.10.5** 热键改键即时生效 + WPF 历史首启自动导入（无 schema 变更）：热键线程原阻塞在 `GetMessageW`（没按键就没消息）→ 改 `MsgWaitForMultipleObjects` + 50ms 超时，改完快捷键立即重注册、失败在设置窗口红字提示；新增 `--import-wpf` 之外的首启自动导入（候选 `%LocalAppData%\ClipboardX\clipboard_history.db` → 同级 `Data/` → `../clipboard/Data/`，标记 `.wpf-import.json`），迁移改走 `wpf::BatchReader` 分批并**补回 WPF 已做的 `ocr_text`**
- **v0.10.6** 反馈通道 + 托盘/设置重组（无 schema 变更）：托盘/后台动作全部走右下角可见提示条（`ui/toast.slint`，长任务带进度条，禁止裸 `return` 收场）；更新下载改 `HttpClient` 分块读出真实进度（替换无流式进度的 `Invoke-WebRequest`）；设置新增「关于」页；托盘菜单 14 项瘦身为高频 6 项；设置六页按语义重排（剪贴板 / 记录与检索 / 常规 / 文件夹跳转 / 高级 / 关于），页码常量收敛为 `logic.rs::PAGE_*` 单一真源，并修「最近进程」列表因页码错位永不加载的存量 bug
- **v0.10.7** 常驻浮层首帧残缺修复（无 schema 变更）：快速查找 / 文件跳转浮层 `hide()`→`show()` 后表头（`everything`/计数）、1px 分隔线、底栏快捷键整块不渲染（软渲染器 `age()==1` → `ReusedBuffer` 只重绘脏区，而 softbuffer Win32 后端不知窗口被隐藏过、`resize()` 同尺寸早退，静态元素永不在脏区）；改**两阶段跨帧抖动尺寸**（显示前抖小 1px → show → 下一帧恢复），强制重建缓冲使脏区=整窗。新增 `--qf-demo <关键词>` 自检开关（两轮同布局会话，产出 `out[-r2][-late].png`）。根因与「`--snapshot` 证明不了此 bug」的验证方法学见 CLAUDE.md「UI 开发陷阱」#9
- **v0.10.8** 更新中心重做（无 schema 变更）：用户报「能检测到新版本但整个更新流程不可用」——`update.rs` 引擎完整，缺的是 UI 状态机。七条症状逐条修：①「发现新版本」文案错指托盘（v0.10.6 已瘦身）→ 改指「设置 → 关于」；②「下载并安装」恒显示且无效 → 加 `update-available` 门控，**仅新版时出现**；③提示误用红框错误样式 → `set_notice` 不再写 `error`，拆成 `error`（红，校验失败）/ `notice`（中性，状态播报）两条独立展示位；④两个更新开关从「常规」页**集中到关于页**；⑤进度从 2 秒自收的提示条改为落关于页的状态行 + 进度条（`progress < 0` 走不确定态）；⑥终态静默 → 统一经 `push_update_state()` 回投；⑦「启动时检查更新」改了不生效 → 保存时比对改动前的值，「关→开」立刻补跑一次后台检查。`AppEvt::UpdateProgress` 加 `done: bool` 显式标终态（决定按钮是否恢复可点）；OCR 拓展包下载从该通道**分离**为独立 `AppEvt::PackNotice`（原先被显示成「正在更新」并误禁用更新按钮）；`UiBtn` 新增 `enabled` 禁用态；新增 `--settings-update-demo <state>` 自检开关（更新 UI 5 种形态，不注入只能拍到 idle）

验收：`cargo check --workspace --all-targets` 零警告；`cargo test --workspace` 确定性全绿；CI 三平台矩阵与 Release 打包流水线均 success。

## 遗留手动验证登记（2026-09-20）

以下项**无法在工具宿主环境闭环**（窗口站权限被裁剪：`SendInput` / `GetCursorPos` / `BitBlt` 全 err=5，见 ARCHITECTURE §9），须在真实交互终端由人点验。此前散落在 M3/M4/M5 各节，现集中登记；新迭代开工前先清这一节。

| # | 来源 | 项 | 入口 |
|---|---|---|---|
| 1 | M3 | 剪贴板 UI 段 F/G/H/J（多屏定位、收藏、右键菜单、自启切换） | `scripts/m3_acceptance.ps1` |
| 2 | M4 | Explorer 内打字呼出 Everything 快速查找、↑↓/翻页/Enter 定位选中 | `scripts/m4_acceptance.ps1` |
| 3 | M5 | 记事本另存为框 Ctrl+G 跳转、前台自动弹出、无框全局模式、WPS 无误触 | `scripts/m5_acceptance.ps1` |
| 4 | v0.10.x | 随包 DLL 与 exe 同目录，剪贴板 / FileJump / QuickFind 日用点验 | 「对齐并超越 WPF 1.9.8」本机手测清单 |
| 5 | 活体 | Everything / FindX 活体查询（已从默认测试集排除，原因见测试注释） | `cargo test -p clipx-everything -- --ignored` |
| 6 | M6a | mac 真机：剪贴板采集、CGEvent 粘贴、LaunchAgents 自启、辅助功能权限 | CI 产出的 mac dmg artifact（未签名，右键打开） |
| 7 | M0 | S2 列表滚动 fps（2000 条下 ≥55fps；内存已达标，风险低） | 手动滚动观察 |
| 8 | v0.10.5 | 热键热更新：设置里改快捷键后**立即生效**（改前默认 `` Ctrl+` `` 被占用也要能换成别的键，不必重启）；注册失败在设置窗口有红字提示 | 设置 → 热键，改完直接按键 |
| 9 | v0.10.5 | WPF 历史首启自动导入：全新安装（空库）启动后能看到老数据，托盘 tooltip 提示「已导入 N 条」 | 装 0.10.5 安装包后首启 |
| 10 | v0.10.6 | 托盘右键（现仅 6 项）：暂停⇄继续、清空两段确认的**动态标签**随状态刷新；各项点击有右下角提示条反馈 | 托盘右键逐项点击 |
| 11 | v0.10.6 | 更新下载/安装进度条可见且走动；设置「关于」页版本/构建/数据目录正确 | 「关于」页 → 检查更新 |
| 12 | v0.10.6 | 「高级」页「最近进程」列表能真正列出进程（修了页码错位导致永不加载的 bug） | 设置 → 高级 → 排除应用 |
| 13 | v0.10.6 | **快速查找 / FileJump 浮层呼出无残缺**：Explorer 内打字呼出后表头（everything/计数）、1px 分隔线、底栏快捷键须**首帧即完整**（修前为 hide→show 复用实例 + 软件渲染器 ReusedBuffer，静态元素整块留白、待结果到达才自愈）；文件跳转浮层同路径一并修 | 真机：Explorer 内打字呼出快速查找 → 观察首帧；Ctrl+G 呼出文件跳转 → 观察首帧 |
| 14 | 待发版 | **更新全链路真机走一遍**：「关于」页点「检查更新」→ 状态行依次出现「正在检查…」/「已是最新」或「发现新版本 …」；有新版时「下载并安装」**才出现**，点击后进度条走动、「更新中…」按钮置灰、完成后恢复；「启动时自动检查更新」从关切到开后**不重启也应触发一次检查** | 真机：设置 → 关于，逐项点验（含关掉 `last_update_tag` 后重试以复现「发现新版本」态） |
| 15 | v0.10.9 | **FIFO/LIFO 批量全链真机走一遍**（此前从无登记，是欠账）：① 热键三态循环 `Off→LIFO→FIFO→Off`；② 入队顺序（FIFO 尾插 / LIFO 头插，重复入队应移位而非忽略）；③ 队首写剪贴板 + 前台 Ctrl+V 逐条推进 + 队空自动回 Off；④ 托盘图标三色 **+ F/L 字母**；⑤ 批量胶囊跟随模式色（与托盘同步）；⑥ 列表选中行底色随模式变 | 真机：连按批量热键切模式，看托盘/胶囊/选中行三处是否同步；按主键+数字入队若干条，切到别的窗口按 Ctrl+V 逐条贴 |
| 16 | v0.10.9 | FIFO/LIFO 三态快照回归：`clipx.exe --no-instance-lock --batch-demo <off\|fifo\|lifo> --snapshot <path>` 三张图（Light/Dark 各一轮），核对托盘色、胶囊色、选中行色 | 真机终端跑上述命令（GUI 快照需真实窗口站，工具宿主跑不了） |
| 17 | v0.10.9 | **老版迁移卡片真机点验**：装过老版的机器上首启 → 关于页出现「老版 ClipboardX 迁移收尾」卡片；点「停用老版开机自启」→ 普通权限下应如实报「需管理员权限」并保留按钮；以管理员运行 clipx 后再点 → Run 值与 `ClipboardX_AutoStart`（含 `_Dev`）应真被删除、卡片收起按钮 | 真机：有老版自启任务的机器上跑，管理员/非管理员各一轮；快照可用 `--settings-page 5 --settings-demo legacy[:running\|:noauto\|:disable] --snapshot <path>`。**已完成**：卡片与失败文案快照已核对；「提权下真删成功」需 UAC 确认，仍未点验（前提 `RunLevel=HighestAvailable` 已核实） |
| 18 | v0.10.9+ | **老版更新通道下发「迁移版」**（路径 A）：发一个 tag > v1.9.9 的包到 `chaojimct/clipboardx`，资产名与包内 exe 名按老更新器硬约束；老用户点「检查更新」即顺通道迁到 clipx | 详见 `docs/migration-research.md`。**launcher 已实现**（老仓库 `Migrator/`，commit 599b454）：`--check`/`--migrate`(退出码 0/2/3/4)/`--demo-busy`，双形态 zip 打包验证通过（no-runtime 6.11MB / SC 68.83MB，包内仅根目录 `ClipboardX.exe`）。**剩余**：老仓库打 tag 发 v1.9.10（真机走老版更新器验证双形态各一轮）+ 精简 flavor 包 |
| 19 | v0.10.9 | **开始菜单前台定位真机点验**：Win11 按 Win 键弹出开始菜单（或 Win+S 搜索）后按呼出热键 → 弹窗应固定在工作区左上 +16px、不压开始菜单、尽量在其之上；`pos_debug.log` 记 `branch=shell-workarea` | 真机：开开始菜单后呼出；自检可用 `clipx.exe --no-instance-lock --shell-demo --snapshot <path>`（强制 Shell 形态，拍渲染 + 查日志分支） |
| 20 | v0.10.9 | **Win+V 副作用回归**：拦截后不应再出现「奇怪的快捷键」/目标应用失焦（Escape 已改为按需注入）；`Data/winv_debug.log` 每条应含 `shell_snagged` + `esc=yes/no`；连按 Win+V 多次，系统开始菜单**不该**闪出 | 真机：WorkBuddy/微信输入框内按 Win+V 多次；按完直接打字，确认焦点仍在输入框；对照 `winv_debug.log` |

已登记、本迭代明确不闭环的欠账（独立议题）：

- **弹窗锚点相对输入框定位仍不稳**：`crates/clipx-app/src/win_popup.rs` 的 `TODO(workbuddy-pos)`。**2026-09-22 已回退 caret2 横向跟光标**（改为 UIA 输入框 BBox 居中 + 修跨屏错屏）；剩余不稳项（UIA 底栏常判 no-composer、对话小窗/首页偶贴地）仍未闭环，**不要再对齐 WPF 定位**，回头单开。

## M6 macOS（预计 2-3 周，拆 M6a-e 五步）

> 执行细化见 docs/CROSSPLATFORM.md（功能块平台矩阵、降级链、风险登记）。要点：M6a 无焦点 NSPanel spike 最先验证（Slint/winit 改造 nonactivating panel，不通过则退化为呼出瞬间获焦模型）；搜索后端统一 findx2-ipc（UDS，协议与 Windows 命名管道同构）；OCR 用 Vision 实现 OcrEngine trait（内存超标预案=独立进程化）；对话框跳转新 crate `clipx-jump`（AX API / Cmd+Shift+G 键盘链）；辅助功能权限 M6a 一次申请。

验收：Mac 上完成与 M1 等价的日用验收。

## M7 Linux 与发布（预计 3 周，拆 M7a-c 三步）

> 执行细化见 docs/CROSSPLATFORM.md。要点：X11 全功能优先（XTest 粘贴/global-hotkey/面板）；Wayland 降级形态明确登记（wl-clipboard 监听 + 无热键/无注入，桌面快捷键绑 CLI 入口）；对话框跳转走 GTK Ctrl+L 键盘链（XTest）；Tesseract OCR 进 AppImage；三平台 `cargo check` 矩阵提前进日常 CI。

验收：三平台 CI 绿灯；每平台过冒烟清单；GitHub Release 发布成功——首发即含完整 Windows 能力（剪贴板 + FileJump + Everything）。

## 风险跟踪

| 风险 | 影响 | 缓解 | 状态 |
|---|---|---|---|
| Slint 弹窗或列表不达标（S1/S2） | 路线返工 | 前置到 M0 首日验证；egui 备案条件已写死 | S1 已通过；S2 内存达标、fps 待手动验证（见「遗留手动验证登记」#7） |
| Wayland 监听与热键 | M7 范围 | 业界普遍难点（CopyQ 在 Sway 亦有 issue）；最坏情况 Wayland 仅支持复制监听 | 接受 |
| OCR 中文质量 | OCR 体验 | 平台原生引擎直调替换路径已定 | **已缓解**：spike 实测 WinRT Media OCR 中文基本不可用（"第三方"→"竺三方"），已加 RapidOCR 拓展包作精度路径（ADR-012）；mac Vision / Linux Tesseract 待 M6/M7 实测 |
| tray-icon 在 Linux 需 GTK loop | Linux 内存 | M7 实测；超标则直连 StatusNotifierItem | 待验证 |
| 单人开发节奏 | 周期 | 里程碑粒度小、每段可日用，随时可停在可用状态 | 纪律约束 |
| FileJump 移植的 Win32 脆弱面（注入/COM/各家管理器私有协议） | M5 | 复用已验证的原生 DLL 与 v1.9.7/1.9.8 行为基线；feature 门控隔离在 clipx-filejump | 数据层 + Picker 已落地，可关 WPF 双进程 |

## 里程碑之外的持续事项

- 每完成一个 M：更新 CHANGELOG、打 tag、发版（沿用 findx 的 CI 模式）
- 文档随代码同步：PRD / ARCHITECTURE / ROADMAP 的变更与代码同一提交，避免文档腐化
- 内存与性能实测数字随每个 M 更新到 PRD §5
- 欠账：WorkBuddy（Electron）热键弹窗相对输入框定位仍不稳（对话小窗会盖聊天、首页偶贴地），见 `crates/clipx-app/src/win_popup.rs` 的 `TODO(workbuddy-pos)`，回头单开；不要再对齐 WPF 定位

### 收尾检查清单（每次发版前逐条过）

v0.10.3 收尾时踩到的坑，固化成清单；顺序即依赖顺序。

1. **警告看全量，不要看日志尾部**：`cargo check --workspace --all-targets 2>&1 | grep -c '^warning'`。v0.10.3 时按 `tail` 误判为 6 条，实际 8 条（`clipx-ocr` 那两条在尾部窗口之外）。
2. **测试要确定性**：`cargo test --workspace` 必须 0 failed。依赖机器状态的活体测试走 `#[ignore]` + 注释里写明手动命令，不要让默认测试集看环境脸色。
3. **版本字串同步四处**：workspace `Cargo.toml` version、`Cargo.lock`（跑一次不带 `--locked` 的构建刷新）、**`scripts/clipx.iss` 的默认 `AppVersion`**、**`.github/workflows/release.yml` 的 `workflow_dispatch` default**（后两处 CI 会覆盖，手工出包时才用，所以最易漏）。打 tag 前必须 bump，tag 名与 version 必须一致。
4. **CHANGELOG 先归位再发版**：`Unreleased` 里若有内容其实已随上个 tag 发出去（v0.10.2 就发生过），先补出该版本段，再为新版本开段。
5. **文档状态头对齐**：PRD / ARCHITECTURE / ROADMAP / CLAUDE.md / README 的「状态：vX · 日期」与项目状态摘要；ROADMAP 另有「v0.10.x 能力增量」列表要补本版本一行；新增 crate 别忘补 §1 结构树与依赖表。
6. **`cargo build --release --locked -p clipx-app`** 必须过，且 `clipx.exe` 与两个 `ClipboardXShellNavigate*.dll` 同目录就位（release workflow 会硬校验这三个产物）。
7. 推 `main` → 看到 CI success；打 **annotated tag** 并推 → 看到 Release success 且 `gh release list` 出现新版本。
8. **文档改动提交前用 `git diff --stat` + 关键行 `grep` 复核**：v0.10.4 定版时 ROADMAP 的「v0.10.x 能力增量」那行其实没落地，提交信息却写了"已补"，下个版本才发现。编辑工具报成功 ≠ 内容真的变了。

