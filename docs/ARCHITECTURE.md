# clipx 技术架构

> 状态：v1.0 · 2026-09-02 · 与 [PRD.md](PRD.md)、[ROADMAP.md](ROADMAP.md) 配套。选型依据来自 2026-09 三路技术调研（Rust GUI 框架、业界剪贴板产品、系统层 crate 生态）。

## 1. 总体结构

单进程、单仓库、Cargo workspace：

```
clipx/
├── crates/
│   ├── clipx-core/       # 纯库：条目模型、去重、搜索语义、拼音、容量策略、配置
│   ├── clipx-store/      # SQLite（WAL + FTS5）、懒加载、迁移
│   ├── clipx-monitor/    # 剪贴板监听：平台抽象 trait + Win/mac/Linux 实现
│   ├── clipx-ocr/        # uniOCR 封装：异步队列、即用即释
│   ├── clipx-filejump/   # M4-M5：FileJump + Everything（仅 Windows，feature 门控）
│   └── clipx-app/        # Slint UI、托盘、热键、装配（薄壳）
├── docs/                 # 本文档集
└── Cargo.toml            # workspace 根
```

| Crate | 依赖规则 | 测试要求 |
|---|---|---|
| clipx-core | 不依赖任何平台 crate、不依赖 UI、不依赖 store | 单测覆盖（去重/搜索/拼音） |
| clipx-store | 依赖 core；rusqlite(bundled) | 内存库单测 + 迁移测试 |
| clipx-monitor | 依赖 core；平台 crate 在平台 feature 后面 | 接口契约测试；平台行为人工验证 |
| clipx-ocr | 依赖 core；uniOCR | 引擎 mock 单测 |
| clipx-filejump（M4-M5） | 依赖 core；windows crate；feature "filejump"，仅 Windows 编译 | 人工回归清单（以 WPF v1.9.8 行为为基线） |
| clipx-app | 依赖以上全部；slint | 冒烟 + 手动验收清单 |

core 与 UI 分离的设计参考 Ringboard 项目（core 为纯库、GUI 只是客户端之一）：若 Slint 撞墙可整体换 egui 而不动 core/store/monitor/ocr，也为未来 CLI/daemon 客户端留门。CopyQ 采用独立监控进程是 Qt"剪贴板必须在 GUI 线程访问"的限制；Rust 监听线程自持消息窗口即可，单进程更省内存。

## 2. 技术选型决策（ADR）

### ADR-001 GUI：Slint 1.17+ 主线，egui 0.36 备案

- 选 Slint 的依据：自实测私有内存 7MB（.NET 基线 96MB）；1.17 起内置系统托盘、虚拟化 ListView、拖放；声明式 UI 利于逐项对齐 WPF 版的弹窗细节；桌面应用 royalty-free 授权。
- 已知风险：slint-ui/slint issue #11133 报告 5 万条滚动存在分配churn（约 20fps）。我们在 2000 条规模 + 固定行高下预期不受影响，M0 spike S2 实测确认。
- 切换条件（提前写死，避免反复摇摆）：M0 结束时若 S1（不抢焦点）或 S2（滚动性能）任一不达标且无 workaround，整体切 egui。egui 路线同样满足内存（同类剪贴板项目实测 8-12MB）与不抢焦点（winit PR #4446 已支持 WS_EX_NOACTIVATE）。
- 被否方案：Tauri v2（WebView2 内存 60MB+ 超标）；Iced（0.14 仍标 experimental）；Xilem / Freya / Dioxus-Blitz（alpha/rc 阶段）；CXX-Qt（体积与授权复杂）；三平台原生 UI 三壳（维护成本三倍，业界没有"全平台 + 低内存"先例恰恰印证此路难走）。

### ADR-002 剪贴板监听：clipboard-rs 0.3.5 + 自建平台抽象

- clipboard-rs 读写 + 监听一体：Windows 事件驱动（WM_CLIPBOARDUPDATE），macOS changeCount 轮询（系统无事件 API，Maccy / PasteBar 的 clipboard-master 同样如此），Linux 走 x11 机制。2026-06 仍在发版。
- 备选：clipboard-master（PasteBar 生产使用）。两个库都隔离在 monitor trait 后面，可替换，不锁定。
- Wayland：M7 用 wl-clipboard-rs（ext-data-control 协议）。
- 原则：monitor 对上只暴露事件流（channel），平台差异不出 crate。

### ADR-003 存储：rusqlite（bundled）+ WAL + FTS5

Ditto、CopyQ、KDE Plasma 6.3 Klipper 均已用 SQLite 存剪贴板历史，这是行业标准答案。bundled 特性免系统依赖（规避 Windows 链接问题）；WAL 支撑读写并发；FTS5 做全文搜索。评估过纯 Rust 的 redb，查询能力不足以支撑搜索场景，否决。

### ADR-004 OCR：Windows 直调 Media OCR（windows crate WinRT），平台 trait 统一接口

原方案 uniOCR，M2 落地时改为 windows crate 直调 WinRT `OcrEngine`：接口面更小（一个 trait + 一个有界队列），免去 uniOCR 的额外依赖与间接层，行为与 ADR-004 备选路径一致。`OcrEngine` trait（recognize_png）保持平台中立，macOS Vision / Linux Tesseract 在 M6/M7 落地时实现同一 trait。后处理移植 WPF 版 OcrTextPostProcessor：CJK 字符间空格剔除、拉丁词间空格保留。引擎上限取 `OcrEngine::MaxImageDimension()`，超限图先等比缩再识别。

### ADR-005 热键与托盘：global-hotkey 0.8 + tray-icon 0.24

均为 tauri 团队维护但不绑定 Tauri，任何 Rust GUI 可用。已知限制：global-hotkey 在 Linux 仅支持 X11（Wayland 热键 M7 另议，候选 hotkey-listener）；tray-icon 在 Linux 需要 GTK 事件循环（M7 实测内存影响，超标则直连 StatusNotifierItem）。

### ADR-006 拼音：核心层实现

tauri 版用前端 pinyin-pro 验证过需求真实性；Rust 版没有前端，拼音在入库时生成（全拼 + 首字母）写入 FTS，搜索时不做全量重算。候选 crate：pinyin 0.11 / inputx-pinyin，M0 定稿。

### ADR-007 版本与发布：沿用 findx 已验证的流水线

tag `v*` 触发 CI 矩阵构建 + 安装包（Windows Inno Setup / macOS dmg / Linux deb+AppImage），发布流程照搬 ../findx 的 GitHub Actions 模式。

### ADR-008 FileJump / Everything：v1 范围外，M4/M5 以独立 Windows crate 吸收，排位先于 macOS

- 这两块是 WPF Full 版的核心能力，但与剪贴板核心正交，且完全 Windows 专属（#32770 对话框生态、TC/XYplorer/DOpus/WPS 集成、Everything 本身均无其他平台对应物）。放进 v1 会拖垮跨平台主线。
- 过渡共存：WPF FileJumpOnly flavor（ClipboardX-filejump.exe，无剪贴板监听）与 clipx 并行，热键不冲突；该 flavor 同时含 Everything 快速查找（同属 CLIPX_FILEJUMP 门控），过渡期功能零缺失。代价是过渡期 Windows 双进程，整机内存目标到 M5 完成才达成。2026-09-02 排位评审（两项功能均日均 10+ 次、Mac 非日常机、跨平台 1.0 无发布硬节点）定为先于 macOS 执行。
- 移植策略分层：注入 DLL（native/ShellNavigate 的 C++ 产物）与宿主语言无关，直接复用；Everything 优先直连其窗口消息 IPC 协议（WM_COPYDATA），省去 Everything64.dll 的 FFI 与分发；宿主侧逻辑（对话框检测、多管理器路径采集、注入调度）用 windows crate 移植。
- 落点：独立 crate clipx-filejump，cargo feature `filejump` 门控（默认关闭），不进入 core/store/monitor/ocr 的依赖图，其编译与否不影响其他平台。
- 行为基线：以 WPF v1.9.8 为回归基线，重点覆盖 v1.9.7/1.9.8 修复集——WPS 纯消息框误判排除规则、BrowseObject 渐进退避重试（0/120/300/600/1000/1500ms）、COM 借用指针（CWM_GETISHELLBROWSER 返回值）不得 Release。最后一条在 Rust 侧 COM 调用中同样致命，列为硬约束。Everything IPC 侧另有一条 WPF 源码注记的坑：搜索串须用 parent: / path: 限定，勿依赖 SetMatchPath，「盘符:\ 关键词」形式实测恒 0 条。
- 回退方案：若移植成本失控，Windows 上长期维持双进程共存（WPF FileJumpOnly 持续维护），代价是放弃 Windows 单进程内存目标。

### ADR-009 渲染器：renderer-software（否决默认 femtovg）

- 实测（M0，Windows 11 + AMD 显卡）：femtovg（OpenGL）release 构建常驻 114MB 工作集——AMD OpenGL 驱动 atio6axx.dll 单模块 66MB；切 renderer-software 后 **19.7MB 工作集 / 4.2MB 私有内存**（2200 条记录加载、弹窗隐藏态），达到 10-30MB 目标区间。
- 弹窗为 480×560 小窗口 + ListView 虚拟化，软件光栅化负载有限；S2 滚动 fps 实测若不达标再议（femtovg 按需恢复编译只需改 feature）。
- 配置：`slint = { default-features = false, features = ["std", "backend-winit", "renderer-software", "compat-1-2", "accessibility", "raw-window-handle-06"] }`。

## 3. 数据流

```
剪贴板 OS 事件
   │  clipx-monitor（平台实现，自持窗口/线程）
   ▼  mpsc channel
clipx-core（去重 blake3 / 格式判定 / 容量策略）
   │  写入
   ▼
clipx-store（entries 元数据行 + payloads 载荷行 + FTS）
   │  事件广播（broadcast channel）
   ▼
clipx-app（Slint 列表增量刷新，只载 preview + 缩略图）

图片条目 ──► clipx-ocr 异步有界队列 ──► ocr_text 回填 store ──► UI 刷新

用户 Enter ──► ClipboardGate 置位 ──► 写剪贴板 ──► 模拟 Ctrl+V ──► Gate 复位
```

线程模型：monitor 线程（每平台一个）、store 单写者线程（mpsc 串行化写入）、OCR 工作线程（有界队列，满则背压丢弃）、UI 主线程（Slint event loop）。channel 模式沿用 WPF 版验证的约定：worker 持 Sender，消费侧持 Receiver，无跨线程共享 Mutex 状态机。

## 4. 数据库设计（v4，M2 定稿）

设计目标：列表查询永不触碰大字段——这是懒加载的根基。

```sql
PRAGMA user_version = 4;

-- 元数据表：列表页只查这张
CREATE TABLE entries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind INTEGER NOT NULL,               -- 0=text 1=image 2=files 3=richtext
  preview TEXT,                        -- 截断预览，列表直读
  content_hash TEXT NOT NULL,          -- blake3
  source_app TEXT,
  pinned INTEGER NOT NULL DEFAULT 0,
  ocr_state INTEGER NOT NULL DEFAULT 0, -- 0=未做 1=进行中 2=完成
  created_ms INTEGER NOT NULL
);
CREATE INDEX idx_entries_created ON entries(created_ms DESC);
CREATE INDEX idx_entries_hash ON entries(content_hash);

-- 载荷表：按需加载，用后释放
CREATE TABLE payloads (
  entry_id INTEGER PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE,
  full_text TEXT,
  html TEXT,
  rtf TEXT,
  file_paths_json TEXT,
  image_mime TEXT,
  image_w INTEGER, image_h INTEGER,
  image_blob BLOB,
  thumb_blob BLOB,                     -- 入库时生成，列表用（宽 64px 等比）
  ocr_text TEXT,                       -- v4 增列：OCR 结果，搜索覆盖
  pinyin_blob TEXT                     -- 全拼连写 + 首字母连写，LIKE 子串匹配
);

-- 全文索引：FTS 仅承担英文/数字词前缀匹配
CREATE VIRTUAL TABLE entries_fts USING fts5(
  entry_id UNINDEXED, text, ocr,
  tokenize = 'unicode61'
);
```

搜索语义（对齐 WPF Contains 行为）：非空查询 = full_text/ocr_text/pinyin_blob 三路 LIKE 子串（拼音 blob 含全拼连写 + 首字母连写，任意位置命中）∪ FTS 词前缀。

容量裁剪双轨（v4）：总条数 max_items 与图片条数 max_image_items 各自独立裁剪（WPF 版 MaxItems / MaxImageItems 语义），pinned 豁免。

设置存 JSON 文件（沿用 WPF 约定），不入库。

**WPF 版数据迁移（M3）**：源为 `Data/clipboard_history.db`，其 `clipboard_history` 表结构已在 WPF 版源码确认，字段映射：

| WPF 列 | 目标 |
|---|---|
| id / entry_type（0=Text 1=Image 2=Files，值与新 kind 兼容） | entries.id / kind，直迁 |
| text_content | payloads.full_text + entries.preview |
| image_blob / image_w / image_h | payloads.image_blob / image_w / image_h（缩略图迁移时生成） |
| file_paths_json | payloads.file_paths_json |
| copied_at_ms | entries.created_ms |
| ocr_text | entries_fts.ocr_text，entries.ocr_state = 2 |

content_hash 迁移时统一计算；重复项按去重规则收敛。迁移是一次性命令（CLI 子命令或设置面板按钮），不在启动路径上。

## 5. 平台矩阵

| 能力 | Windows | macOS | Linux X11 | Linux Wayland |
|---|---|---|---|---|
| 监听 | 事件（clipboard-rs） | changeCount 轮询 500ms | clipboard-rs / x11 | wl-clipboard-rs（M7） |
| 热键 | global-hotkey | global-hotkey | global-hotkey | M7 另议 |
| 托盘 | tray-icon | tray-icon | tray-icon（GTK loop） | 同左 |
| 不抢焦点弹窗 | WS_EX_NOACTIVATE（raw-window-handle 取 HWND 后 SetWindowLongPtr，S1 spike） | NSPanel nonactivating 风格 | override-redirect / 不激活 hint | M7 验证 |
| 粘贴模拟 | SendInput Ctrl+V | CGEvent Cmd+V | XTest Ctrl+V | M7 |
| OCR | Media OCR（uniOCR） | Vision（uniOCR） | Tesseract（uniOCR） | 同左 |
| 自启 | 计划任务 XML 导入 | LoginItems（SMAppService） | .desktop autostart | 同左 |
| FileJump / Everything（ADR-008） | M4-M5 移植吸收 | 不做 | 不做 | 不做 |
| 里程碑 | M0-M5 | M6 | M7 | M7 |

自启注意（WPF v1.9.7 教训）：Windows 用 schtasks XML 导入方式注册——路径不加引号、无执行时限、允许电池供电、限定当前用户，避免弹黑窗与任务失效。

## 6. Slint 集成要点

- 无焦点弹窗：Slint 桌面后端基于 winit；窗口创建后经 raw-window-handle 取 HWND 补 `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` 样式。M0 第一件事（S1）。
- **键盘输入不走窗口焦点**（WPF 版对齐）：弹窗永不取焦点，呼出时装 WH_KEYBOARD_LL 低级钩子拦截按键（M0：Esc；M1：搜索/方向键/回车），隐藏时卸载。实测 AttachThreadInput+SetFocus 方案会抢前台（S1 失败），已否决。
- **事件循环**：`run_event_loop_until_quit()`（等价 WPF 版 `ShutdownMode="OnExplicitShutdown"`）；注意 `ComponentHandle::run()` 内部会先 show() 窗口，托盘常驻应用不可用。
- ListView：固定行高；VecModel 只承载 preview/缩略图等轻字段，行内不放载荷对象。2000 条滚动实测（S2）。
- 主题：亮/暗/跟随系统三态，Slint palette + 自定义 token。
- 弹窗定位：光标所在显示器，沿用 WPF 版规则（无效矩形回退鼠标位置）。

## 7. 关键机制（WPF 版经验移植，硬约束）

1. **懒加载生命周期**：图片 blob 只存在于两个时刻——store 的行内、预览或粘贴的瞬间。列表与预览取数后立即释放，不进任何长生命周期缓存。
2. **ClipboardGate**：写回剪贴板前置位、延时复位；监听侧丢弃 Gate 窗口内的事件，杜绝历史自环。
3. **OCR 即用即释**：图片 bytes 送引擎后立即 drop；OCR 队列有界，满则丢弃并标记重试，不允许堆积。
4. **强哈希去重**：blake3。tauri 版用标准库弱哈希（DefaultHasher）做图片去重是已知教训。
5. **回填纪律**：任何"回填/预热"逻辑不得触碰懒加载字段，否则内存曲线失控（WPF 版 EnqueueBackfill 教训）。
6. **错误处理**：边界代码禁止裸 unwrap 与静默 .ok()；剪贴板 / OCR / IO 失败一律降级为日志 + 功能开关，进程不崩（tauri 版静默吞错是反面教材）。

## 8. 风险与 Spike

| 编号 | 验证项 | 时机 | 通过标准 | 不通过怎么办 |
|---|---|---|---|---|
| S1 | Slint 无焦点弹窗 | M0 首日 | 热键弹出后原应用焦点不丢、弹窗可键盘操作 | 排查 Slint 平台 API 缺口；仍不可行则触发 ADR-001 切 egui |
| S2 | Slint 2000 条 ListView 滚动 | M0 | 稳定 ≥55fps，内存无单调增长 | 行高分页/窗口化降级；严重则切 egui |
| S3 | clipboard-rs 可靠性 | M0 | 连续复制 100 次（文本/图片/混合）无漏采、无重复风暴 | 换 clipboard-master（接口隔离在 trait 后） |
| S4 | uniOCR 中文质量 | M2 | 常用截图文字人工评估可用 | Windows 直调 Media OCR / macOS 直调 Vision，接口不变 |

**Spike 结果（2026-09-02，M0）：S1 通过**——弹窗呼出前后前台窗口不变（WS_EX_NOACTIVATE 生效），Esc 经 WH_KEYBOARD_LL 钩子关闭且被吞掉不漏给前台应用；AttachThreadInput+SetFocus 方案实测抢前台，已否决改走钩子。**S3 通过**——100 连发 100/100 入库、0 重复（写入方偶发 1-3 次 OpenClipboard 争抢，均被重试化解，采集侧零丢失）。**S2 部分验证**——内存达标（19.7MB 常驻/27.5MB 连发峰值，目标 ≤30MB）；滚动 fps 需人工验证，留待 M0 手动清单。
