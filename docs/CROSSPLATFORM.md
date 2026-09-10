# clipx 跨平台实现方案（M6/M7 执行版）

> 状态：v1.0 · 2026-09-10 · 本文是 ROADMAP M6/M7 的执行细化，功能块按「平台最优形态 + 降级链 + 工作量」展开。

## 0. 总原则

1. **每平台做到该平台的最优形态**，不追求三平台实现逐字节一致；交互语义对齐（呼出/搜索/粘贴/跳转），实现细节各走各的最优路径。
2. **Windows 现有路径冻结为基线**，新平台代码全部进 `#[cfg]` 门控的平台模块，禁止改动 Windows 行为。
3. **搜索后端统一 findx2-ipc**（JSON 行协议，named pipe / UDS 三平台同构）；Everything WM_COPYDATA 降级为「Windows 无 FindX 时的回退」。
4. **权限前置**：macOS 辅助功能权限是 QuickFind 键盘监听、粘贴模拟、对话框跳转的共同依赖，M6 首个 spike 就申请，不推迟。
5. 内存硬约束（≤30MB 常驻）在 mac 上单独实测——Vision/objc 框架加载可能突破，超标则 OCR 改独立进程按需拉起。

## 1. 功能块方案

### 1.1 剪贴板核心（采集/存储/搜索/预览/粘贴）

| 平台 | 采集 | 粘贴回写 | 图片格式 |
|---|---|---|---|
| Windows | clipboard-rs 事件驱动（现有） | SendInput Ctrl+V + ClipboardGate（现有） | DIB+PNG 双写（现有） |
| macOS | clipboard-rs changeCount 500ms 轮询 | CGEvent Cmd+V（辅助功能权限） | NSPasteboard TIFF/PNG；写回用 clipboard-rs 统一 |
| Linux X11 | clipboard-rs（x11 机制） | XTest Ctrl+V | PNG 为主；WPS/LibreOffice 逐家实测 |
| Wayland | wl-clipboard-rs（ext-data-control） | 无键盘注入：改为「写入剪贴板 + 用户自粘」或桌面快捷键触发 CLI | PNG |

- 采集侧进 `clipx-monitor::platform`（trait 已就位，非 Windows 目前 bail）。
- 粘贴模拟抽 `paste::PlatformPaste` trait：Windows SendInput / macOS CGEvent / Linux XTest。**Wayland 明确降级**：面板提供「复制」按钮与 Enter=复制模式，不做模拟。
- 图片回写：mac 走 NSPasteboard 多格式（PNG+TIFF），Linux 只保证 PNG + text/uri-list（文件）。DIB 是 Windows 概念，不移植。
- 富文本 HTML Format：Windows 私有注册表格式；mac/Linux 剪贴板 HTML 直接是 text/html——采集与回写按平台格式归一化进 core（kind=3 的载荷统一存 HTML 文本）。

### 1.2 OCR

| 平台 | 引擎 | 说明 |
|---|---|---|
| Windows | Media OCR 直调（现有） | 不动 |
| macOS | Vision `VNRecognizeTextRequest`（objc2/vision） | 实现 `OcrEngine::recognize_png` 同一 trait； RecognitionLevel 用 accurate；语言 hw/zh 请求列表 |
| Linux | Tesseract（leptessulation/tesseract-rs）或 CLI 子进程 | 二进制依赖进打包（AppImage 打包 tessdata）；中文包 chi_sim 可选下载 |

- 有界队列、即用即释、OCR 后释放图片 bytes 的纪律原样保留（CLAUDE.md 硬约束）。
- 内存预算：Vision 首次调用拉起框架约 +20-40MB，**必须实测**；超标则 OCR 独立进程化（请求经本地 socket，主进程零加载）。

### 1.3 QuickFind（搜索 + 跳转）

- **搜索后端统一 findx2-ipc**：`findx_pipe.rs` 扩展为三平台客户端——Windows 命名管道（现有）+ macOS `~/Library/Application Support/FindX/findx2.sock` + Linux `$XDG_RUNTIME_DIR/findx2.sock`。协议同一 JSON 行（`Search{query,pinyin,limit,offset}`），客户端零协议改动，只加传输层分支。findx2-service 未运行时：mac/Linux 提示拉起 `-startup`（与 Windows 现有逻辑对齐）。
- **Everything WM_COPYDATA 保留为 Windows 回退**（无 findx 时），现有协商逻辑不动。
- **入口差异（关键）**：Windows「Explorer 打字即搜」依赖 WH_KEYBOARD_LL，无跨平台等价物。跨平台形态改为**热键呼出式浮层**：全局热键唤起 QuickFind 浮层 → 输入 → findx 结果 → Enter 跳转。UI 完全复用现有 Slint 浮层，仅入口从钩子拦截改为热键。
  - macOS 全局键盘监听（NSEvent addGlobalMonitorForEvents，辅助功能权限）作为「可选增强」留待后续；基础形态只靠热键。
  - Linux X11：global-hotkey 可用；Wayland 无全局热键——依赖桌面自定义快捷键绑定到 `clipx --qf`（CLI 入口，见 1.6）。
- **结果跳转**：系统命令层，三平台现成——
  - macOS：`open -R <path>`（Finder 中显示）/ `open <path>`
  - Linux：`nautilus --select <path>` / `dolphin --select <path>` / `xdg-open`
  - Windows：现有 Shell COM Navigate / SHOpenFolderAndSelectItems

### 1.4 对话框跳转（Windows FileJump 的跨平台版 → 新平台模块 `clipx-jump`）

与 `clipx-filejump`（Windows 专属，ADR-008 冻结）并列，新建 `clipx-jump` crate 承载 mac/Linux 的对话框跳转，trait 分层对齐（检测 / 采集 / 注入）：

| 平台 | 对话框检测 | 路径注入 | 路径采集（当前管理器） | 降级链 |
|---|---|---|---|---|
| macOS | AX API：NSOpenPanel/NSSavePanel role/subrole 特征（系统统一控件，比 Windows #32770 简单） | ① AXUIElement 直接对路径元素 SetValue（最稳）② 键盘链 Cmd+Shift+G → 键入 → Return | Finder：AppleScript `POSIX path of insertion location`；标签页/窗口枚举经 System Events | AX 失败→键盘链→「复制路径」 |
| Linux X11 | X11 窗口属性（WM_CLASS/_NET_WM_PID）+ 标题启发式（打开/保存/另存为） | GTK 对话框：XTest 键入 Ctrl+L → 路径 → Return；Qt 非 native 对话框：fileNameEdit 直接键入 | xdotool 取活动窗口 + 桌面环境 DBus（可选） | 键盘链→「复制路径」 |
| Wayland | 不做检测 | **无键盘注入能力**（平台安全模型） | 不做 | 仅「复制路径到剪贴板」 |

- Windows 保持 `clipx-filejump` 现有最优路径（ShellNavigate DLL 注入），不迁移到 clipx-jump；两 crate 共享「收藏/最近路径」设置键。
- 辅助功能权限（mac）是本块的硬前置，与 1.3/1.5 同一次授权。

### 1.5 热键 / 托盘 / 无焦点面板 / 键盘路由

这是三平台差异最大的块，需要一次**架构抽象**：

- `KeySource` trait 抽象键盘事件来源：
  - Windows：WH_KEYBOARD_LL 钩子（现有，面板永不获焦）
  - macOS：**面板成为 keyWindow**（NSPanel nonactivating styleMask，不抢前台应用焦点但可接收键盘）+ 全局热键呼出；NSEvent 全局监听仅用于「Esc/点击外部关闭」类增强
  - Linux X11：global-hotkey 呼出 + 面板获焦（X11 无全局钩子需求，面板正常取焦即可，失焦关闭已有）
- **无焦点面板 spike（M6a，最高风险）**：Slint/winit 的 macOS 后端默认 NSWindow 会激活。验证路径：objc2 运行时将 winit 创建的 NSWindow 替换/改造为 NSPanel 并设 `StyleMask::NONACTIVATING_PANEL`、`becomesKeyOnlyIfNeeded`。若 Slint 输入事件管线在该形态下正常，则 mac 交互模型成立；不成立则退化为「面板短暂获焦」模型（呼出瞬间抢焦点，失焦关闭）——可用但体验降级，需登记 PRD §8。
- 托盘：tray-icon 三平台（ADR-005）；Linux 需 GTK 事件循环，实测内存，超标直连 StatusNotifierItem。
- 托盘菜单/图标变色（FIFO/LIFO）：跨平台通用，tray-icon API 一致。

### 1.6 自启 / 更新检查

| 平台 | 自启 | 更新 |
|---|---|---|
| Windows | schtasks XML（现有） | GitHub Releases 静默检查（现有） |
| macOS | SMAppService（LoginItems） | 同左；dmg 内 Sparkle 不引入，保持静默检查+打开下载页 |
| Linux | `.desktop` 文件写入 `~/.config/autostart` | 同左 |

### 1.7 打包与 CI（沿用 findx 流水线，ADR-007）

| 平台 | 产物 | 签名 |
|---|---|---|
| Windows | Inno Setup（现有 `scripts/clipx.iss`，含 ShellNavigate DLL） | 可选 codesigning |
| macOS | dmg（bundle：clipx.app + Info.plist LSUIElement=0） | Developer ID + notarization；无账号先出未签名包（ROADMAP 预留） |
| Linux | deb + AppImage（AppImage 捆绑 tesseract + tessdata） | 无 |

- CI 矩阵：`windows-latest` / `macos-latest` / `ubuntu-latest`，tag `v*` 触发；**跨平台 `cargo check` 矩阵提前到日常 CI**（见 §2），不要等发版才发现平台渗漏。

## 2. 架构改造（现在就做，Windows 零行为变化）

1. `clipx-app/Cargo.toml`：`windows = "0.59"`、`clipboard-rs` 等移入 `[target.'cfg(windows)'.dependencies]`（mac/Linux 编译第一步就挂在这）。
2. `main.rs`：Windows-only 的 mod 声明加 `#[cfg(windows)]`（keyboard_hook / mouse_hook / win_popup / explorer_* / wic 等逐个复核）；`logic.rs` 33 处 cfg 审计为 KeySource/能力 trait。
3. 新建 `KeySource` trait + `Paste` trait（§1.1/1.5），Windows 实现即现有代码平移。
4. `clipx-everything` 更名/扩为 `clipx-search`：findx2-ipc 客户端（UDS）为主、Everything 回退（Windows）。
5. CI 加 `cargo check --target aarch64-apple-darwin` / `x86_64-unknown-linux-gnu`（无需真机）。
6. `win_popup::find_window_by_title` 等 Win32 细节全部收在 cfg 内（已完成）。

## 3. 里程碑计划

### M6 macOS（2-3 周，拆五个可验收步）

| 步 | 内容 | 验收 |
|---|---|---|
| M6a spike | 无焦点 NSPanel + 全局热键 + 辅助功能权限申请 | 面板呼出不抢前台焦点、可键盘操作（S1 的 mac 版） |
| M6b 核心 | 轮询采集 + 存储 + 搜索 + 面板 + 粘贴（CGEvent）+ 托盘 + 自启 | mac 上达到 M1 等价日用验收 |
| M6c OCR | Vision 实现 OcrEngine；内存实测 | 图中文字可搜；常驻 ≤30MB（超标则进程化） |
| M6d 跳转 | findx UDS 接入 + QuickFind 热键浮层 + `open -R` + AX 对话框跳转 | 打开/保存框跳目录；Finder 定位 |
| M6e 打包 | dmg + 签名公证（或未签名） | 安装可用 |

### M7 Linux 与发布（3 周）

| 步 | 内容 | 验收 |
|---|---|---|
| M7a | X11 全功能（采集/粘贴/热键/面板/托盘） | X11 日用验收 |
| M7b | Wayland：wl-clipboard 监听 + 降级交互（无热键/无注入） | 降级形态可用并文档化 |
| M7c | 对话框跳转（GTK Ctrl+L 键盘链）+ OCR Tesseract + 三平台 CI + deb/AppImage | 首发 release 三平台过冒烟 |

## 4. 权限与系统能力清单（macOS）

| 权限/能力 | 用途 | 申请时机 |
|---|---|---|
| 辅助功能（Accessibility） | 键盘监听增强、CGEvent 粘贴、AX 对话框跳转 | M6a 首次引导 |
| Apple Events（Finder） | 当前路径采集 | M6d 首次使用时 |
| 登录项 | 自启 | 设置开启时 |

## 5. 风险登记

| 风险 | 影响 | 缓解 | 状态 |
|---|---|---|---|
| Slint/winit 无法改造为 nonactivating NSPanel | mac 交互模型降级 | M6a 最先验证；退化方案=呼出瞬间获焦模型，登记 PRD §8 | 待验证（最高优先） |
| Vision 框架加载突破 30MB | M6 内存硬约束 | 独立进程化预案 | 待实测 |
| findx2-service 在 mac/Linux 的分发（用户需装两个东西） | 部署复杂度 | 方案 a：clipx 打包捆绑 findx2-service；方案 b：未运行时提示一键拉起 | 决策点：M6d |
| Linux 各文件管理器/对话框工具链碎片化 | 跳转覆盖率 | 只承诺 GNOME/KDE 主流路径，其余走「复制路径」降级 | 接受 |
| Wayland 无热键/注入 | M7 功能面 | 降级形态文档化，与 CopyQ 同水位 | 接受（ADR-002/005 已登记） |
| 单人节奏 | 周期 | 每步独立可验收，随时可停在日用状态 | 纪律约束 |
