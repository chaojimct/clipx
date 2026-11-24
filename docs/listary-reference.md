# Listary 参考手册（clipx 开发用，自包含）

日期：2026-11-24 ｜ 状态：可直接开工 ｜ 跨仓索引：findx `docs/research/t120,t121,t122`
目标：把 Listary 6（文件搜索 + Explorer/对话框集成的标杆）验证过的思路，逐项复用到 clipx。
本文件自包含：新会话只读本文即可开工，不必回找聊天记录。

---

## 1. Listary 6 架构速览（逆向实证）

分析对象：`Listary.exe` 安装包（Inno Setup 6.2.1，内含构建 2025-08，即 Listary 6.3 当期版本；用户重装后为 `C:\Program Files\Listary` 6.3.5.94，复核为同一构建）。
方法：静默解包 → Rust/UniFFI 符号提取 → ilspycmd 反编译全部 .NET 程序集 → PE 导出表解析。

| 组件 | 技术 | 职责 |
|---|---|---|
| `listary_engine.dll`（14MB） | Rust，经 UniFFI 暴露 21 个方法，106 个 PE 导出 | 索引 + 搜索（`open_session/search/get_search_results/release_session/cancel_request`、`add/remove/rebuild/poll/wait_for_indexers`、`get_indexer_infos/diagnostics`） |
| `Listary.exe`（2MB） | WPF / .NET Framework 4.8 | UI + 20+ `ISearchProvider` 扇出 |
| `Listary.Service.exe`（31KB） | SYSTEM 服务，WCF `net.pipe://localhost/Listary/ListaryService` | 特权句柄中介：`CreateFile(\\.\C:)` 后 `DuplicateHandle` 给引擎进程；`CreateProcessAsUser`（处理 UAC 分裂 token）拉起 Hook 宿主 |
| `DeviceIOControlLib.dll` | 通用封装 | Disk/Volume/USN/MFT 等 raw IO |
| `cnmatch.bin`（62KB） | 拼音表：u16le 个数 422 + 422 个长度前缀音节串（`a,ai,an…`），后跟 ~60KB 字→音映射 | 全拼/首字母/模糊音数据源 |
| `Hooks/ListaryHook{32,64}-6.1.10.1.dll` | 全局 `SetWindowsHookExW` + `OLEACC`(MSAA)，**独立版本号，ABI 稳定** | Explorer + 经典对话框钩子 |
| `ListaryHookHost{32,64}.exe` | 按位数常驻（`SetTimer` 事件循环），Service 以正确会话 token 拉起 | 钩子宿主 |
| `FileAppPlugins/{Explzh,Files,OneCommander,WinRAR.ExtractDialog,WPS.FileDialog}/` | 见 §3 | 按应用插件 |

`listary-core` 模块树（panic 路径还原）：`db/{builder,file_db,hard_link,priority,read_only,serialize,snapshot,transaction,writer}`、
`indexer/ntfs/{mft_reader,volume_monitor_thread,volume_change,list_nfts_volumes}`、`indexer/folder/{mod,watcher,schedule,id_pool}`、
`indexed_search/{service,score_based_searcher,path_filter}`、
`string_matcher/{bm_searcher,keyword_string_matcher,fuzzy_string_matcher,pinyin_searcher,fuzzy_pinyin_searcher}`、
`fuzzy/position_bitmap`、`pinyin/mod`、`unicode/mod`。依赖：`rust-rocksdb`、`uniffi_core 0.28.3`、`aho-corasick`、`memchr`、
`dashmap/rayon/crossbeam`，自研 `me-hashbrown`、`snapshot-hashmap`。

核心设计（clipx 逐项对照用）：
- **索引**：`$MFT::$DATA` extent 直读全量 + USN 增量（`Volume{serial_number,label,root}` 跟踪卷）；RocksDB 存储；
  `FileNode{flags,file_name,parent_id,last_write_time}` + 内存 `FolderTreeNode{first_child,next_sibling,priority}`（路径拼装不查库）。
- **速度**：会话式增量搜索 + 分页（`PaginationParams{offset,length}`）+ 全链路取消；
  匹配器分档（`StringMatcherType{Keyword,Fuzzy}`，BM/aho-corasick/memchr/位并行 `position_bitmap`）；
  零拷贝 `IStringPiece` + `OnlyAscii` 快路径 + `RemoveDiacritics`；自研 `ParallelQuickSort`（阈值 8192）；
  `CombinedScore(ulong)` = match 分|频率|新近|目录亲和；`PriorFolders`、`OrderedFolderParts` 分段匹配；参数全部下推引擎。
- **拼音**：`pinyin_searcher` + `fuzzy_pinyin_searcher`（z/zh、n/ng），UI 只透传 Query。
- **优先级**：`FilePriorityRules` + 三规则（路径/属性/文件名正则）× 五档 `Ignored/Low/Normal/Internal/High`，
  UI 侧 `DiskSearchPriority{High=1,Normal=0,Low=-1,Ignored=-2}`，`PriorityRules` 经 protobuf 同步到引擎。

## 2. clipx 复用总表（12 项）

| # | Listary 资产 | clipx 落点 | 现状差距（clipx 仓内证据） | 改动要点 | 验收 |
|---|---|---|---|---|---|
| 1 | `CombinedScore(u64)` 融合排序 | `crates/clipx-store/src/lib.rs` 历史搜索 | `scored: Vec<(u8,EntryMeta)>` 只有"前缀0/其他1"两档（`:1173-1185`） | 新 `match‖freq‖recency` 三段 u64 分；`entry` 加 `use_count/last_used`，打开即+1 | 高频/刚用过排前 5；有打分文档 |
| 2 | 会话式增量 + 取消 | `clipx-app` 搜索 UI（`explorer_quickfind.rs` 60KB / `win_popup.rs` 196KB）逐键 SQL | 每键一次全量 `LIKE`，无 session 复用；取消待 `win_popup` 细查确认 | 同 session query-extend 复用上次结果集；新击键发 cancel；store 查询支持中途退出 | 连续输入总耗时 <1.5×首查；旧查询 5ms 内丢弃 |
| 3 | 多音字（字→多音节） | `crates/clipx-core/src/pinyin.rs::to_pinyin_blob` | `text.to_pinyin()` 每字只取一个读音（`:16`） | blob 存"主读音+次读音"；`text_matches_query`/`pinyin_hit_spans` 同构跟进（硬要求不变） | 任一读音可召回；高亮不错位 |
| 4 | 模糊音 z/zh、n/ng | 同上 | `pinyin` crate `plain()` 疑无模糊（先实测确认） | 查询侧声母归一化 + blob 侧可选展开 | `zhuang` 召回"装"；矩阵钉住 |
| 5 | 匹配器分档 + `position_bitmap` fuzzy | 剪贴板模糊搜索 | 只有子串匹配（原文子串 OR blob 子串，`pinyin.rs:37-51`） | Keyword（BM/memchr）/Fuzzy（位并行）/拼音三档；短查询直扫 | 拼错 1 字仍召回；短查 P50 <8ms |
| 6 | 零拷贝 + `OnlyAscii` 快路径 | `text_matches_query` 热路径 | 每次调用 `to_lowercase()` + 重算 blob（`pinyin.rs:42-43`），逐键重复分配 | 纯 ASCII 走字节比较；blob/小写只算一次（随 entry 缓存；DB 侧已有 `pinyin_blob` 列，内存侧对齐） | 热路径 0 大分配（profiling 证明） |
| 7 | 优先级三规则 × 五档 | `clipx-filejump` Candidate 排序 | `Candidate{path,alias,source,is_file}` + `CandidateSource{Manager/Favorite/Recent/Last/Everything/Local/Current}`（`filejump/models.rs:4-50`）无权重 | source 基分 + 路径/类型正则规则（复用 `custom.rs::CustomRule` 存储形态）；`Ignored` 类不进候选 | 常用目录/收藏排前；规则热生效 |
| 8 | 特权服务中介句柄 | `filejump/inject.rs` 远程线程注入 | `CreateRemoteThread(LoadLibraryW)` 从用户进程直接注入（`:206-221`），高完整性/提权目标必然失败 | SYSTEM 服务代开目标进程句柄 + `DuplicateHandle` 回注；`CreateProcessAsUser` 处理会话隔离（抄 ListaryService 全套） | 提权进程/高 IL 对话框可导航；失败降级键盘回退（现有 `Alt+D` 链保留） |
| 9 | 双 Ctrl 热键 | 不做（2026-11-24 结论：现有 `Ctrl+\``/`Ctrl+G` 已够用；双击判定多一台状态机，`Ctrl+C/V` 误触面大，不为迁移 Listary 加复杂度） | — | — | — |
| 10 | 下推参数/剪枝 | `clipx-everything`（Everything/findx 双后端） | `findx_pipe.rs` 已有 findx 客户端 + 双层失败冷却（`FAIL_UNTIL` 10s / `ABSENT_UNTIL` 5min） | filejump `Local` 源改走 findx pipe（`pinyin:true`），`Everything` 保留回退；`Current/Recent/Favorite` 不动 | 中文跳转召回率看齐 findx；findx 缺席 0 延迟回退 |
| 11 | `FolderTreeNode` 内存树 | 不适用（无目录树语义） | — | 明确不做 | — |
| 12 | RocksDB 存储 | 不适用（万级历史 SQLite 完全够用，FTS+触发器已成熟） | — | 明确不换库；只借 WAL/快照纪律（store 已有 v1→v8 迁移链） | — |

## 3. Explorer/对话框集成实现路径（filejump 专项，Listary 五层 vs clipx 两层半）

### 3.1 Listary 五层（逆向实证，反编译结论见 §6）

1. **UI/引擎**：搜索 + Picker，不直接碰目标窗口。
2. **插件宿主**（`Listary.FileAppPlugin.Host.dll`，已反编译）：扫描 `FileAppPlugins/*/ListaryPluginManifest.json`
   `{ManifestVersion,Id,PluginType,PluginVersion,Authors,Description,Homepage,Icon,Api:NetFramework,ApiVersion,Entry}`；
   能力查询（`CanOpenFolder/CanOpenFolderAndSelectFile/CanWindowGetTabs/CanTabGetFolder/CanTabOpenFile…`）；
   宿主服务（`PostMessage/SendCopyData/SendMessage/GetWindowText/SetWindowText` 全经 `IFileAppPluginHost`，可 mock 可测）。
3. **按应用插件**（一目录一 DLL + manifest），三种写法：
   - **Win32 控件 ID 链**（`WinRAR.ExtractDialog`）：`#32770`+进程名+`GetDlgItem(12320)==SysTabControl32` 绑定；
     `FindWindowEx` 嵌套 `#32770` → `GetDlgItem(102/101/1001)` 定位路径编辑框；读 `GetWindowText`、写 `WM_SETTEXT(12)`。
     插件源码公开：`github.com/listary/Listary.FileAppPlugin.WinRAR`（可直接读，比逆向快）。
   - **UIA 直驱 FlaUI UIA3**（`WPS.FileDialog`）：`UIA3Automation.FromHandle(hWnd)`，`ClassName=="KcfdFileDialog"` 绑定；
     `FindFirstDescendant(ByClassName("KcfdFilterWidget"))` → `KcfdComboBox` → `QLineEdit`（回退 `kd::KDTextField`）`.AsTextBox()`。
     **要点：WPS 这种 Qt 自绘框 Listary 也不注入，直接 UIA 对 Qt 类名**。`Files`（WinUI3）、`OneCommander` 同理全 UIA。
     插件源码公开：`github.com/listary/Listary.FileAppPlugin.WPS`、
     `github.com/files-community/Listary.FileAppPlugin.Files`（社区在维护）、`github.com/listary/Listary.FileAppPlugin.OneCommander`、
     第三方 `github.com/wany-oh/Listary.FileAppPlugin.Explzh`——**新管理器=新插件，不动主程序**。
   - **Explorer**：shell COM（`IShellFolder/IShellItem/SHOpenFolderAndSelectItems`）+ 钩子。
4. **钩子层**：`Hooks/ListaryHook{32,64}-6.1.10.1.dll`（独立版本号，ABI 稳定），
   `SetWindowsHookExW/CallNextHookEx/UnhookWindowsHookEx` 全局钩子，链接 `OLEACC`（MSAA 探查对话框/控件）；
   `ListaryHookHost{32,64}.exe` 按位数常驻（`SetTimer` 事件循环），Service 以正确会话 token 拉起
   （`CreateProcessAsUser` 处理 UAC 分裂 token）。
5. **服务层**：`ListaryService`（SYSTEM）代开特权句柄 + 拉起 Hook 宿主（本表 #8）。

### 3.2 clipx filejump 缺口 → 参考动作

| 缺口（`clipx-filejump` 探查确认） | Listary 参考 | clipx 动作 |
|---|---|---|
| 无 tab 枚举、无选中项（`collectors.rs:46-76` 只采当前路径） | `IFileWindow.GetCurrentTab/IGetFileTabs`、`IOpenFolderAndSelectFile` 是一等接口 | `Candidate` 加 tab/选中项；Explorer 用 `SelectedItems`，对话框用 ListView 选中 |
| UIA 二档被砍（头注释"二档 UIA 白名单"只剩 `explorer_edit_fallback`）、WPS/Custom 永不注入只能键盘回退（`inject.rs:53-101`） | WPS/Qt/WinUI **根本不注入**，UIA 直驱 Qt 类名（`KcfdFileDialog/QLineEdit` 现成抄） | Rust 加 UIA 路径（`uiautomation` crate），WPS 从"键盘回退"升级为"UIA 直驱"；Files/OneCommander（WinUI3）新增 UIA 采集 |
| 400ms 轮询前景对话框（`clipx-app/src/filejump.rs:790-826`） | 全局钩子（WH_SHELL/WinEvent 思想）+ MSAA；轮询只做兜底 | 加事件驱动（`SetWinEventHook` 前景/创建事件），400ms 轮询降为 fallback |
| 按应用特判散落风险 | 插件外置 + manifest + 能力查询；**插件源码全开源，社区可贡献**（Files 插件就是社区的） | `custom.rs::CustomRule` 升级为能力接口（`IFileWindow/IFileTab/IOpenFolder` 对等物）；新应用=新文件+manifest，不改主链 |
| 键盘回退与直写顺序 | 先直写（`WM_SETTEXT`/UIA `ValuePattern`），键盘（`Alt+D→Ctrl+A→键入→Enter→校验`+`Ctrl+L` 追加）只是顽固框 fallback | clipx 已是该顺序（`set_filename_edit` 优先），保持；把"写后回读校验"制度化 |
| 钩子 DLL 与主程序耦合 | 钩子独立版本号（6.1.10.1），ABI 稳定 | `native/ShellNavigate` 明确版本 + 锁定导出表（`ClipboardX_RemoteNavigate/RemoteReadCurrentFolder` 不许随意改签名） |

### 3.3 开源捷径（不用逆向，直接读）

- `github.com/listary/Listary.FileAppPlugin.WPS`（Qt 类名/UIA 条件）
- `github.com/listary/Listary.FileAppPlugin.OneCommander`
- `github.com/listary/Listary.FileAppPlugin.WinRAR`（控件 ID 链）
- `github.com/files-community/Listary.FileAppPlugin.Files`（社区维护，WinUI3 UIA）
- `github.com/wany-oh/Listary.FileAppPlugin.Explzh`（第三方范例：新管理器=新插件不动主程序）
- clipx filejump 注入现状：`inject.rs:4,206-221`
  （`OpenProcess→VirtualAllocEx→WriteProcessMemory→CreateRemoteThread(LoadLibraryW)→导出函数`，
  目标为前台 `#32770` 所属进程，DLL `ClipboardXShellNavigate(.dll/32.dll)`，
  远调 `ClipboardX_RemoteNavigate/RemoteReadCurrentFolder`，
  `RETRY_DELAYS_MS=[0,150,300,500,800,1200]`，WPS/Custom 永 `SimulateOnly`）；
  采集现状：`collectors.rs`（Explorer `CabinetWClass/ExploreWClass` + COM `IShellWindows`，
  失败回退 `EnumChildWindows` 取最长现存目录文本；TC `SendMessage(1075,2029/2030)` 借剪贴板；
  XY `WM_COPYDATA`；DOpus `dopusrt.exe /info`，活动/被动双路径）；
  对话框分类：`dialog.rs:12-23,32-239`（System/Wps/Custom/NotDialog，IDMan 主界面/mstsc 登录框/保存确认框排除）；
  联动：`zorder_linked_folder(delta=2)`（`:730-740`）。

## 4. clipx 已有、勿重构（验证过，动了是倒退）

- `pinyin_blob` + `LIKE` + 触发器同步 + 检索高亮同构（`store/lib.rs:39-72`，`core/pinyin.rs:58-80`）——只补多音/模糊（#3/#4）。
- `findx_pipe` 双层失败冷却——X 宇宙跨进程调用范本；findx 侧取消机制可照抄该思路。
- `quirks.rs` 按应用策略表 + 冻结线（`host/quirks.rs`：微信/企微/QQ/终端/Chromium/壳宿主）——闭源适配资产的正确形态，新增特判只加表行。
- `inject.rs` 调度骨架（`plan_for/RETRY_DELAYS_MS/navigate_to_folder` + 键盘回退链 + WPS 永不注入策略）——只换句柄来源（#8）。
- `collectors.rs` 多管理器采集——覆盖度已超 Listary 公开能力，继续加宿主即可。
- `jump` crate（`clipx-jump/src/lib.rs`：`reveal/open` 跨平台统一入口）——findx 搜索结果"定位/打开"直接用。
- `dock.rs::dock_position`、`models.rs`、`custom.rs` 纯函数——可被 findx 直接复用的通用基础设施（不依赖剪贴板业务）。

## 5. Windows 底层能力的 clipx 应用（2026-11-24 深思考：剪贴板/弹窗场景）

§2–§3 是"功能对功能"的映射；本节是"Windows 底层机制往剪贴板场景硬套"，有 4 个新结论
（app 亲和排序、UIAccess 破 UIPI、事件驱动代替轮询、WAL 即免费快照），以及 1 个隐私维度的 Ignored 规则。

### 5.1 输入路径：UIPI 是剪贴板工具真正的护城河

- 现状：`paste.rs`（36KB）+ `keyboard_hook/mouse_hook`（WH_KEYBOARD_LL 低级钩子，已有）负责粘贴回写。
  粘贴到**提权窗口**（任务管理器、管理员控制台）会被 UIPI 拦截 `SendInput/PostMessage`——这是所有剪贴板工具的共性天花板，
  Listary 同样受限（它的 service 解决的是**会话/token**问题，不是 UIPI）。
- 参考动作（按序）：① 先实测当前粘贴在高 IL 窗口的失败模式并登记 quirks 表（延续冻结线）；
  ② **UIAccess 路线**：签名二进制 + `uiAccess=true` manifest，可与提权窗口交互（仍进不了 Secure Desktop，这是全行业已知上限，如实告知用户）；
  ③ 服务中介只解决"句柄/会话"类问题（本表 #8），**不解决输入注入**，不要混为一谈。
- 本表 #9（双 Ctrl）已结论为不做，不占输入侧工作。

### 5.2 感知路径：事件驱动代替 400ms 轮询

- 现状：`clipx-app/src/filejump.rs:790-826` 400ms 轮询 `resolve_from_foreground` 跟踪焦点对话框；另有 `WH_GETMESSAGE` 信标只给 TSF/光标用。
- Listary 参考：全局钩子（WH_SHELL/WinEvent 思想）+ MSAA（`OLEACC` 链接实证）做"变化感知"，轮询只做兜底。
- clipx 动作：加 `SetWinEventHook`（`EVENT_SYSTEM_FOREGROUND` + `EVENT_OBJECT_CREATE` 对 `#32770` 类）做主链，
  400ms 轮询降级为 fallback；CPU/电池收益用实测说话（笔记本场景）。
- TSF vs MSAA：`tsf_caret/tsf_inject/vault_inline` 是重武器，保留；简单 `Edit` 控件加 MSAA/`GetCaretPos` 快路径，
  先 profile 再动手（本表 #5/#6 同原则）。

### 5.3 排序：app 亲和（新想法，Listary `PriorFolders` 的剪贴板类比）

- Listary 把"当前目录"加权进 `CombinedScore`；剪贴板没有目录，但有**前台应用**。
- 新规则：在本表 #1 的 u64 分里加一段"来源应用 == 当前前台应用"加权——在 VS Code 里呼出，VS Code 复制过的东西排前。
  数据来源就是 `quirks.rs` 的 exe 识别（已归一化 stem，直接复用）。
- 这是 Listary 没有的心智（"越用越准" + "在哪用哪准"），写进打分文档可解释。

### 5.4 存储：SQLite WAL 就是免费的读快照（明确不折腾）

- Listary 用 RocksDB + `read_only.rs` 解决"搜索不被索引阻塞"；clipx 的 SQLite 若开 WAL（M0 已验证 WAL 零丢失），
  读天然走快照隔离——**已经免费拥有本表 #6 要的东西**，只需确认 `journal_mode=WAL` + 读事务不长持。
- 动作：① 确认 WAL + 读事务纪律；② 加 `use_count/last_used` 列（本表 #1）；③ FTS/触发器不动。
  规模不到 10 万条之前，任何换库讨论都是伪命题。

### 5.5 隐私：Ignored 规则是剪贴板版的优先级（高优先级）

- Listary 五档里最狠的是 `Ignored`（直接不进索引）；剪贴板的等价物是**不记录名单**：密码管理器、银行、特定应用/窗口标题正则。
- clipx 动作：`ClipboardGate` 旁加规则引擎（应用 stem + 标题正则 + 内容模式），命中直接丢弃、不入库；
  另加自动清理（过期/条数上限）。这是合规与口碑线，排在花哨功能前面。
- 来源：Listary `FileNameRegexPriorityRule{regex,apply_to_files,apply_to_folders}` + `DiskSearchPriority.Ignored`。

### 5.6 性能基线先行（findx `perf_suite` 对等物）

- findx 有 `perf_suite` 九轮中位数基线；clipx 开工前先建 harness：弹窗逐键 P50/P99、搜索耗时 vs 历史条数、
  粘贴端到端、启动、常驻内存。没有它，§2 的验收线全是空话。
- 位置建议：`clipx-app/examples` 或 `scripts/` 验收脚本延续（M0/M1 有 `scripts/m0_acceptance.ps1` precedent）。

### 5.7 clipx 修订执行顺序（2026-11-24 修订版，取代旧顺序）

0. 先收敛再开新：Unreleased 的密码库 A–I 先发版（已大到装不下，禁叠新功能）→
1. 性能 harness（§5.6，先测量）→ 2. 排序融合（本表 #1 `use_count/last_used` + §5.3 app 亲和；体感最强，schema v10+ 配 ADR）→
   3. 隐私 Ignore 规则（§5.5；剪贴板全记录，合规是口碑线，提前）→ 4. 拼音多音/模糊音（本表 #3/#4；召回正确性）→
   5. 增量/取消/虚拟化（本表 #2；scale 到 10 万条）→ 6. UIA 直驱 WPS + 事件驱动（§5.2 + 本表 #7；filejump 专项）→
   7. findx 下推/剪枝（本表 #10）→ 8. 热路径零拷贝/Top-K（本表 #5/#6；profiling 后）→
   9. UIAccess/特权中介（本表 #8；动安装/签名，最后）。
- 出界重申：双 Ctrl（本表 #9，已结论不做）、RocksDB、MFT/USN（filejump Local 源除外）、FolderTree、遥测——明确不做。
- 注：此前版本 §5 的旧顺序（排序→双 Ctrl→拼音→增量→权重→联调→特权→性能）已被本 §5.7 取代；
  旧"动前验证"三项（win_popup 链、pinyin 模糊音、uiautomation spike）仍有效，见本节各小节；
  旧 filejump 穿插计划并入 §5.7 第 5 步；本仓纪律（单仓/对齐 WPF/里程碑可日用/UI 不进 core）不变，见 ROADMAP。

## 6. 证据来源总表（新会话按图索骥）

| 来源 | 位置 | 内容 |
|---|---|---|
| Listary 安装包 | `C:\Users\chaoj\Downloads\Listary.exe`（SHA-256 `c62249e0…a0288`） | 原始分析对象 |
| Listary 可用安装 | 已按要求卸载（2026-11-24），用户将自行重装后重新反编译；结论已固化于本文 §1/§3 | Hook DLL、插件、引擎全套 |
| 插件反编译结论 | 已提炼入本文 §3，不再保留产物；WPS UIA 类名链、WinRAR 控件 ID 链原文见上文 §3.1–§3.2 | — |
| findx 逆向报告 T120 | findx 仓 `docs/research/t120-listary6-reverse-engineering-report.md` | 引擎/索引/速度/拼音全文 |
| findx 超越计划 T121 | findx 仓 `docs/research/t121-surpass-listary-plan.md` | P0–P4 + P1-0 spike 设计（`bench_mft` 五方案对比） |
| 跨仓映射 T122 | findx 仓 `docs/research/t122-listary-to-clipx-mapping.md` | 本文件的姊妹篇（含 X 宇宙备忘） |
| clipx 探查结论 | `filejump/{inject,collectors,dialog,dock,custom,models}.rs`，`host/quirks.rs`，`everything/findx_pipe.rs`，`jump/lib.rs`，`core/pinyin.rs`，`store/lib.rs`，`app/{keyboard_hook,win_popup,explorer_quickfind,filejump,paste}.rs` | 行号见上文各表 |
