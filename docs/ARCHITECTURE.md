# clipx 技术架构

> 状态：v1.1 · 2026-09-03 · 与 [PRD.md](PRD.md)、[ROADMAP.md](ROADMAP.md) 配套。选型依据来自 2026-09 三路技术调研（Rust GUI 框架、业界剪贴板产品、系统层 crate 生态）。

## 1. 总体结构

单进程、单仓库、Cargo workspace：

```
clipx/
├── crates/
│   ├── clipx-core/       # 纯库：条目模型、去重、搜索语义、拼音、容量策略、配置
│   ├── clipx-store/      # SQLite（WAL + FTS5）、懒加载、迁移
│   ├── clipx-monitor/    # 剪贴板监听：平台抽象 trait + Win/mac/Linux 实现
│   ├── clipx-ocr/        # uniOCR 封装：异步队列、即用即释
│   ├── clipx-everything/ # M4：Everything WM_COPYDATA IPC + 检索表达式（仅 Windows）
│   ├── clipx-filejump/   # M5：FileJump（仅 Windows，feature 门控）
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
| clipx-everything（M4） | 无 UI 依赖；windows crate（WM_COPYDATA） | 包布局单测 + live 查询（服务在场时） |
| clipx-filejump（M5） | 依赖 core；windows crate；feature "filejump"，仅 Windows 编译 | 人工回归清单（以 WPF v1.9.8 行为为基线） |
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

### ADR-010 文档型文件预览：新纯库 crate `clipx-doc`，文本摘录优先

- 文件条目预览此前只显示路径。`clipx-doc` 做类型判定 + 文本摘录 + 元数据卡（纯库，不依赖 UI，可供以后 CLI/daemon 复用；core/store/monitor/ocr 依赖图不变）。
- 分层落地完毕：文本嗅探（扩展名白名单 + 无 NUL 内容嗅探，UTF-8/BOM/UTF-16LE/GBK 回退）+ 元数据卡 + 打开/定位（复用 `clipx-jump::open_path/reveal`）；表格（`calamine` 纯 Rust，首表转 TSV，前 30 行）；docx/pptx 手解（`zip` 取 `w:t`/`a:t`，段落转行，pptx 按页码数字排序，不引 writer 向的 `docx-rs`）；PDF 文本（`pdf-extract`，只提文本不渲染——`pdfium` 因 ~15MB 原生二进制 + FFI/打包成本明确否决，`hayro` 待成熟再评估）。新依赖：calamine / zip（deflate）/ pdf-extract / encoding_rs，均为纯 Rust。
- 内存纪律：单文件 raw 上限 64KB，字符截断复用预览 48k 上限，preview worker 线程内解析、不进缓存，读后即释；文件内容不进 FTS（只预览不索引，无 schema 变更）。

### ADR-011 OCR 行框（P1a，PixPin 式选行复制前置）

- PixPin 机制确认：本地 OCR 取词级坐标框 → 图上叠不可见可框选文本层 → 命中框文字拼接复制。我们的三平台引擎天然给框（WinRT `OcrWord.BoundingRect` / Vision `boundingBox` / Tesseract TSV），此前链路只取 `Text()` 丢了几何。
- 分两步，均已落地：P1a 行列表——引擎新增 `recognize_lines`（全文 + 行级框，归一化 0-1，单图上限 200 行），`payloads.ocr_boxes` 存行框 JSON（v7 迁移，旧行 NULL 由回填补框后收敛），预览图片下方行列表单击复制该行（双击大图复制全文）；P1b 图上叠加层——Slint 按 contain+zoom/pan 把框投到显示坐标（图片像素尺寸经 `preview-img-w/h` 下发），悬停高亮，单击框复制该行，空白处框选多行按阅读序拼接复制（`PreviewCopyOcrRect`，中心命中判定，未命中/点选不吞剪贴板），放大后叠加层让位给平移。
- P1b-2（词级 + 常显）：`OcrLine.words` 存词框（单图 1200 词上限，旧行框 JSON 无 words 字段向前兼容）；叠加层改渲染词盒（中文词多为 1-3 字，约等于按字选），单击词复制该词，框选按词命中、同行 CJK 感知拼接（`postprocess::join_words`，行内全中直接用行文本）；盒子放大后仍显示可点——单击 4px 阈值防误触，放大后从盒起手的拖拽经 `pan()` 回调平移，1x 下从盒起手的大拖拽放弃点击（框选请从空白起手）。
- 真文本选择：预览正文用只读 TextInput（Slint 原生拖选 + Ctrl+C + 选区高亮），替代不可选中的 Text 与 P1a 行列表（全文=各行拼接，行复制走图上框）；聚焦时钩子只留 Esc/Enter，其余走 Slint 原生（`PreviewTextFocus`）。
- 图上文本层（微信预览/PixPin 贴图式）：原图干净展示，高亮直接画在原字上；
字上按下=选词，空白处按下=框选（1x）/平移（放大时，场景图自身命中测试）；
左键松开只定选区（单击取框/点空清选区），选区留存，右键/复制条/Ctrl+C 才复制
（弹窗选区保留，可连续复制）；无橡皮筋，拖动中命中词实时变蓝即反馈；
Ctrl+C 钩子上报不吞（TextInput 原生复制继续，逻辑层互斥）。

### ADR-012 OCR 拓展包：RapidOCR ONNX（PP-OCRv6 small），默认不集成

- 动因（spike 实测，11 张狗食截图）：WinRT Media OCR 中文基本不可用（"第三方"→"竺三方"，"蘢讎"类乱码）；RapidOCR 行级整行正确（置信度 0.94-1.00）。PixPin 本机验证同样路线（`onnxruntime.dll` + 60MB+ 模型 + 自研管线）。
- 形态：cargo feature `ocr-rapid`（`clipx-ocr/rapid`）默认关闭——ort 静态链接约 +25MB 体积，默认构建不受影响；`rapidocr-core 0.2.2`（ort 2.0，image 0.25 同版本，无 cv2）。
- 调度：`AutoOcrEngine`（常驻）按任务决策——拓展包优先（session 按任务新建、用完即弃，冷加载 ~256ms），失败/缺模型回退 Media OCR；无 feature 构建上 rapid 选项回退 media。切换引擎需重启（引擎在队列线程持有）。
- 模型包：`Data/ocr-models/`（随 Data 双模式），`ppocrv6-small` 4 件约 32MB，清单取 rapidocr-core 注册表（URL+SHA256，不自建第二份）；设置 `ocr_engine=auto/rapid` 且缺模型时后台自动下载（复用更新通道的 powershell/curl 手法，SHA256 校验），完成提示重启生效；并发守卫进程内单例。
- 框数据：rapidocr 原生给行四边形（像素坐标、阅读序），取轴对齐外接框归一化；纯 CJK 行按字均分伪词框（全角等宽近似准），含拉丁只给行框——现有叠加层/行列表零改动消费。
- 内存纪律：常驻零增长（session 不驻留）；单任务峰值 ~350MB（release 实测，arena 默认关闭、单线程），DetInputLimits 默认 4MP 下缩；超大图沿用 max_dim 预缩放。
- 平台：Windows（rapid 主/media 备）、macOS（Vision，不动）、Linux（rapid 替代 Tesseract 计划）。
- 代价：磁盘 +~45MB（exe +25，模型 +32）；构建多 ort 编译（约数分钟）；模型源 ModelScope（国内可达，海外待验证，失败回退 media）。
- trait 兼容：`recognize_lines` 有默认实现（调 `recognize_png`、行框为空），mac/Linux 后续实现同一方法即可；`set_ocr_text` 保持纯文本语义（不碰框列），worker 改走 `set_ocr_result`。

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

## 4. 数据库设计（v7，OCR 行框）

设计目标：列表查询永不触碰大字段——这是懒加载的根基。

```sql
PRAGMA user_version = 7;

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
  ocr_boxes TEXT,                      -- v7 增列：OCR 行框 JSON（P1a），NULL=无框旧数据
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
7. **剪贴板原子快照**（M3 教训）：类型判定与数据读取必须在**同一次 OpenClipboard 周期**内完成——clipboard-rs 逐格式独立开合，在变更瞬间与 rdpclip 等并发监听方竞争 open，重试弱时会把富文本误判成纯文本。持有期间 open 还可能被第三方粗暴 CloseClipboard 打断（实测约 1% 概率，GetClipboardData 报 ERROR_CLIPBOARD_NOT_OPEN）：读取中途失败**不可降级**（会把含 HTML 的条目存成 kind=0），必须放弃本次结果、整体重开重读。
8. **Everything IPC**（M4）：查询走 `EVERYTHING_IPC_COPYDATAQUERYW(2)`。Everything 1.4 的 QUERYW 头全是 DWORD（`reply_hwnd` 4 字节，搜索串 @20）；1.5 起 HWND/ULONG_PTR 为指针宽（搜索串 @28）；findx2-service 字段顺序又不同。按 1.4 → 1.5 → findx 空串探测并缓存，1.4 包发给 1.4 服务端会因 `reply_copydata_message` 错位而不回包（表现为超时）。窗口类匹配允许 1.5 Alpha 实例后缀。Everything 以服务跑在 session 0 时本会话无 IPC 窗口——查询前可 `-startup` 拉起用户态托盘客户端；仍不可达则快速查找用当前文件夹 `read_dir` 兜底。搜索串用 `parent:` / `path:` 限定，勿用「盘符:\ 关键词」。

## 8. 风险与 Spike

| 编号 | 验证项 | 时机 | 通过标准 | 不通过怎么办 |
|---|---|---|---|---|
| S1 | Slint 无焦点弹窗 | M0 首日 | 热键弹出后原应用焦点不丢、弹窗可键盘操作 | 排查 Slint 平台 API 缺口；仍不可行则触发 ADR-001 切 egui |
| S2 | Slint 2000 条 ListView 滚动 | M0 | 稳定 ≥55fps，内存无单调增长 | 行高分页/窗口化降级；严重则切 egui |
| S3 | clipboard-rs 可靠性 | M0 | 连续复制 100 次（文本/图片/混合）无漏采、无重复风暴 | 换 clipboard-master（接口隔离在 trait 后） |
| S4 | uniOCR 中文质量 | M2 | 常用截图文字人工评估可用 | Windows 直调 Media OCR / macOS 直调 Vision，接口不变 |

**Spike 结果（2026-09-02，M0）：S1 通过**——弹窗呼出前后前台窗口不变（WS_EX_NOACTIVATE 生效），Esc 经 WH_KEYBOARD_LL 钩子关闭且被吞掉不漏给前台应用；AttachThreadInput+SetFocus 方案实测抢前台，已否决改走钩子。**S3 通过**——100 连发 100/100 入库、0 重复（写入方偶发 1-3 次 OpenClipboard 争抢，均被重试化解，采集侧零丢失）。**S2 部分验证**——内存达标（19.7MB 常驻/27.5MB 连发峰值，目标 ≤30MB）；滚动 fps 需人工验证，留待 M0 手动清单。

## 9. 验证环境约束（M3 教训）

UI 自动化验收（SendInput 注入、屏幕截图）**不能在 TRAE 工具宿主等受限进程内执行**：这类进程的窗口站权限被系统性裁剪——SendInput / GetCursorPos / BitBlt / GetForegroundWindow 全部失败（err=5 或返回 0），而 EnumWindows / OpenClipboard / schtasks / GetDC 不受影响。症状与锁屏/安全桌面**完全一致**，极易误诊（M3 首轮即误判为锁屏）。判别法：qwinsta 确认本会话 Active、LogonUI 属于另一会话（控制台）后注入仍被拒，即为执行环境受限而非锁屏。

纪律：

- 涉及 UI 注入/截屏的验收段（如 m3 脚本的 F/G/H/J 段）必须在**用户自己的交互终端**运行；工具宿主内只跑剪贴板/数据库/进程类段
- 验收脚本以 SendInput 无副作用探针自动分段（[0] 段），受限环境输出 PARTIAL（exit 2）而非 FAIL
- 程序化 UI 验证可走 `--uitest` 参数（启动即显示弹窗，绕过热键依赖），但截图动作本身仍需交互会话
