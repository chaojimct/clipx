# Changelog

本项目遵循里程碑发版（见 docs/ROADMAP.md），tag `v*` 触发 CI。

## Unreleased

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
