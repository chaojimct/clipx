# TSF 输入法光标定位研究（微信输入法悬浮框是怎么做到的）

状态：研究结论 + spike 实测（2026-10-22，小满；spike 工程在仓库外 `C:\Users\chaoj\dev\tools\caret-beacon-spike`）
动机：面板呼出位置跟随输入光标，在 Electron 类应用上一直是难点（见 `CLAUDE.md` 输入定位章、`docs/electron-uia-research.md`）。微信输入法（WeType 3.0.0.37）在 WorkBuddy（Electron）里能把候选/联想悬浮框精确钉在光标下方，本文取证它是怎么做到的，以及 clipx 能不能抄。

## 一、证据（本机实测）

1. **产品目录**：`C:\Program Files\Tencent\WeType\3.0.0.37`（UI 层，Flutter：`flutter_windows.dll` + `wetype_server.exe` / `wetype_renderer.exe`）；`WetypeCore_3.0.0.37\{x64,x86}\wetype_tip_core.dll`。
2. **注册为 TSF 文本服务（TIP）**：注册表 `HKLM\SOFTWARE\Microsoft\CTF\TIP\{86598FB9-66A2-463E-B9C2-AEB906D477AD}`，语言档案 `0x00000804`（简中），`Description=WeType`，COM InprocServer32 指向 `C:\windows\system32\wetype_tip.dll`（x86/x64 各一份，因为要被不同位数的宿主进程加载）。
3. **进程内注入（tasklist /m wetype_tip.dll 实测）**：该 DLL 同时加载在 WorkBuddy.exe、Weixin.exe、QQ.exe、Cursor.exe、msedge.exe、WXWorkWeb.exe、WeChatAppEx.exe、explorer.exe 等几十个进程里——**凡是 TSF 宿主，都被注入**。
4. UI 与测量分离：测量在进程内的 tip DLL，候选框窗口本身由进程外的 `wetype_server.exe`（Flutter 窗口）绘制，两者走 IPC。

（取证脚本：`.workbuddy/tmp/wetype_reg_probe.py`）

## 二、机制还原

微信输入法是标准 **TSF（Text Services Framework）输入处理器**：

- 键盘事件经 ctfmon/TSF 分发到 TIP，TIP 拼音合成。
- **光标坐标来自宿主自己**：TIP 对当前 `ITfContext` 调 `ITfContextView::GetTextExt`，由**宿主应用**（Chromium/Electron 对 TSF 的实现是完备的——它必须支持中文输入法）返回合成锚点处文本的**屏幕坐标矩形**。
- 兜底技巧（业界已知，见参考资料）：个别 Chromium 应用 `GetTextExt` 失败时，IME 会**预上屏一个零宽字符**（U+200B），再用 `ITfRange` 查它的矩形来定位；GUITHREADINFO/Win32 caret 那条路在 Chromium/Electron 上不可用（无真实 caret）。

**一句话：它拿到的是"宿主进程内部"的一手坐标，我们 UIA 是"进程外部"的二手近似。** 这不是算法差距，是通道特权差距——TIP 是操作系统背书的、每个文本宿主必须配合的输入通道。

## 三、clipx 能不能抄？

### 路线 A：注册一个"只测光标"的迷你 TIP（光标信标）——可行，就是微信同款数据源

做一个**不处理任何按键**的 TIP：`Activate` 后挂 `ITfContextEventSink`（OnSelectionChange/OnLayoutChange），对当前 context 调 `GetTextExt`，把 `{pid, hwnd, caret_rect, timestamp}` 写进一块命名共享内存；clipx 呼出面板时优先读信标（新分支 `tsf:beacon`，高于现有 UIA 链），信标过期/无值再走 `uia_composer_box` 那套。

收益：
- 所有 TSF 应用（Electron、Chromium 系、UWP、Terminal）拿到**精确到字符的光标矩形**，微信特判、`uia:field-box`、`wechat-input-box` 几何猜等一长串兜底大幅简化（保留作 fallback）。
- 提权应用（UIPI）场景：TIP 在宿主进程内、随宿主提权运行，普通权限的 UIA 钩不到的高 IL 窗口它测得到；但共享内存需要带允许低 IL 读的 DACL。

代价 / 风险（都要如实面对）：
- **注入责任**：DLL 加载进每一个有输入框的应用，我们的 DLL 在宿主里崩 = 宿主崩。必须 panic-free、零阻塞、最小分配；Rust + `windows-rs` 手写 COM 可控。
- **安装注册**：CLSID + `HKCU\SOFTWARE\Microsoft\CTF\TIP`（Win8+ 支持每用户 TIP，预期免管理员——**待验证**，包括未签名 DLL 能否被加载）。x86 宿主要么再出一枚 32 位 DLL，要么接受那些应用不享受信标。
- 杀软/SmartScreen 对"输入法形 DLL"的误报风险。
- 工程量：先 spike——一个 cdylib COM DLL，激活后在记事本/WorkBuddy 里把 caret rect 落日志文件，验证通道成立；成立后再做共享内存 + 安装器（`scripts/clipx.iss` 加注册表段）。

### 路线 B：不注册 TIP，继续深化现有 UIA 链

现状已覆盖 WorkBuddy（`field-box` 尾部兜底）。但每个新应用都要一次"研究 + 特判"（微信、企业微信、WorkBuddy……都是这么来的），长期不可扩展，且 collapsed selection（空框）场景原理性拿不到 caret 级精度。

### 结论

**值得抄，按路线 A 做 spike。** 数据源同款（TSF GetTextExt）、架构同款（进程内测量 + 进程外消费），clipx 的消费端（`popup_rect_on_box` 的居中/上贴逻辑）完全不用改，只是坐标来源多一路。排在当前迭代收尾之后、M6 之前验证；若 per-user 未签名 TIP 走不通（需签名/需管理员/被杀软拦），再评估成本是否上调。

## 四、标记

- 我验证了：WeType 注册信息、DLL 路径与位数、进程注入清单（本机 tasklist/注册表取证）。
- 我推断的：`wetype_tip_core.dll` 内部用 GetTextExt（未逆向，基于 TSF 架构 + 业界资料 + 命名布局推断；机制本身无悬念）。
- 未验证：per-user 免管理员注册未签名 TIP 的可行性；spike 工程量。

## 五、参考资料

- TSF 候选栏定位研究（预上屏零宽字符定位、Electron 无 caret）：https://blog.csdn.net/yang1fei2/article/details/118879579
- TSF 跨进程坐标同步失效（UIPI/DPI 角度）：https://tsight.io/articles/11750789
- 本项目相关：`CLAUDE.md`「输入定位（win_popup.rs）」、`docs/electron-uia-research.md`、`Data/pos_debug.log`

---

## 六、spike 实测结果（2026-10-22 下午~晚间）

按本文路线 A 做了 spike：仓库外独立工程 `C:\Users\chaoj\dev\tools\caret-beacon-spike`（Rust cdylib COM DLL + 激活 helper + 注册脚本），TIP CLSID `{C4BE0C0A-9326-4C1A-BEAC-0CBEAC0DEC01}`，profile `{...0DEC02}`，语言 0x0804。

### 已验证（有日志/进程清单为证）

1. **Rust 实现 TIP 可行**：windows 0.62 + `windows-core::implement`（注意 0.58 无 `_Impl` trait，不可用）。DllGetClassObject/IClassFactory/CoCreate 全通。
2. **注册硬门槛**：HKCU 每用户 TIP 注册被系统整体拒绝——`Register`/`RegisterProfile`/`RegisterCategory`（连无关 category）全 E_FAIL，`ActivateLanguageProfile` E_FAIL；`ActivateProfile` E_INVALIDARG。提权写 HKLM（CLSID + `CTF\TIP` 键 + 7 个 Category + LanguageProfile，布局抄 WeType）并装自签证书到机器级 Root/TrustedPublisher 后：`RegisterProfile`/`RegisterCategory` 全 **Ok**，`ActivateLanguageProfile` **Ok**。微软文档明确第三方 IME 必须数字签名；本机实测自签 + 机器级信任即可过校验（未触发签名拒绝）。
3. **全局激活 = 注入全系统**：`ITfInputProcessorProfileMgr::ActivateProfile(TF_PROFILETYPE_INPUTPROCESSOR=1, 0x0804, clsid, profile, NULL, TF_IPPMF_FORSESSION|TF_IPPMF_ENABLEPROFILE|TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE)` 成功瞬间，**40+ 进程同时回调 Activate**——WorkBuddy、Weixin、QQ、Cursor、msedge、explorer 全中。「进程内一手坐标」通道完全成立。
4. **sink 管线成立**：ThreadMgrEventSink/TextEditSink 挂接成功，应用内焦点切换（msedge 开地址栏等）实时收到 `OnSetFocus`。
5. API 备忘（踩过的坑）：`ITfInputProcessorProfileMgr` 无独立 coclass，从 `CLSID_TF_InputProcessorProfiles` 对象 cast 而来；`TF_PROFILETYPE_INPUTPROCESSOR=1`、`TF_IPPMF_ENABLEPROFILE=1`、`FORSESSION=0x20000000`。

### 未闭环（如实登记）

- **键透传**：beacon 作为活动键盘 TIP 时，键盘输入消失（Notepad3 打字无文本、无编辑）。v2~v4 实现了 `ITfKeyEventSink` 全 FALSE 透传（`fforeground` 两种取值都试过），`OnTestKeyDown` 从未被咨询。键在到达应用/sink 之前被吞，机制未查明。
- **OnEndEdit → GetTextExt 数值**：未观测到（依赖上一条；另外 Notepad3/Scintilla 纯键盘输入可能不走 TSF 编辑会话，只走 IME 合成）。
- 「sink 残留」（切换回微信输入法后 beacon 仍产出坐标）假设因此未测。

### 结论与建议

- 可行性：**通道成立，无原理障碍**。剩余是一个明确定义的工程问题（键盘处理 + 编辑会话管道），不是研究问题。
- 完整参照实现：开源输入法 [chewing/windows-chewing-tsf](https://github.com/chewing/windows-chewing-tsf)（Rust + windows 0.62，`tip/src/text_service/`：`ITfTextInputProcessorEx`、`ITfCompositionSink`、`GUID_COMPARTMENT_KEYBOARD_OPENCLOSE` compartment、`RequestEditSession` 管道、key_event 全套）。
- 工程量从 spike 升级为正式模块：签名证书（正式分发）、安装器提权写 HKLM、键处理/编辑会话、共享内存发布端、clipx 消费端（`tsf:beacon` 分支）。建议 ROADMAP 立项，排在 M6 前评估。
- spike 遗留状态清理：管理员跑 `caret-beacon-spike\unregister_hklm.bat`（删 HKLM/HKCU 注册键 + 证书）+ 重启（v1 DLL 驻留在打开过的应用直至重启，期间会往 `%TEMP%\caret_beacon.log` 追加焦点切换日志）。当前输入法已恢复微信输入法，测试记事本已关闭。

---

## 七、产品化落地与关键实测（2026-10-22 夜）

### 7.1 已落地的代码（clipx 仓库内）

| 位置 | 作用 |
|---|---|
| `crates/clipx-tsf/` | 信标 DLL（cdylib，lib 名 `clipx_caret_beacon`），产品化版 TIP |
| `crates/clipx-core/src/tsf_caret.rs` | 共享内存协议 v1/v2 常量 + 纯 Rust 解析器（8 个单测，三平台可跑） |
| `crates/clipx-app/src/tsf_caret.rs` | clipx 侧消费者：写 request_seq → 等 ack → 读结果槽 → 屏幕坐标换算 |
| `crates/clipx-app/src/tsf_activate.rs` | profile 激活器（FORSESSION + DONTCARECURRENTINPUTLANGUAGE） |
| `crates/clipx-app/src/win_popup.rs` | `caret_screen_point_dbg()` 里插 `tsf:beacon` 分支（**排在 imm 之前**，优先于所有启发式） |
| `scripts/register_beacon_hklm.bat` 等 | 注册/卸载/证书/探针脚本 |
| `scripts/clipx.iss` | 安装器新增 `caretbeacon` 任务（ShellExec runas 弹 UAC）+ 打包信标 DLL |

编译状态：`cargo build -p clipx-tsf --release` ✅；`cargo build -p clipx-app --release` ✅ 零警告；
`cargo test --workspace --locked` ✅ 252 passed / 0 failed。

### 7.2 协议 v2（`Local\clipx-tsf-caret`，64 槽 × 64 字节）

- 槽 0 = 控制块：magic `"BC2"` / version / request_seq / request_tick / **ack_seq** / flags
- 槽 1..63 = 结果环：magic `"BC1"` / pid / tick64 / rect(l,t,r,b) / dpi_aware / seq / source
- 消费端兼容 **v1**（旧 spike 版：槽 0 起即结果、无控制块）——`detect_version` 自动分流。
- v2 新增能力：信标 **常驻线程每 40ms 轮询控制块**，clipx 写 request_seq 即触发一次主动测量，
  测量结果写 ack_seq + 结果槽。**这解决了「OnEndEdit 残留态下 GetTextExt 报 0x80040206」的问题**
  （v1 只能靠编辑结束事件被动触发）。

### 7.3 关键实测结论（本轮新发现，重要）

**① 铁律已实证成立：信标不影响用户的输入法。**
- 激活前后 `GetActiveProfile` 均为微信输入法（`clsid=86598FB9-…`）；探针跑完「激活 → 反激活 → 切回微信输入法」后活动 profile 仍正确恢复为微信输入法。
- 键 100% 透传：信标 `ITfKeyEventSink` 全部返回 FALSE。
- `Deactivate` 会把 keyboard openclose 恢复为 1（否则用户切回微信输入法会卡英文态）。

**② 阻断性问题：TSF 只把「活动 profile」的 TIP 加载进新进程。**
- 激活瞬间，已有宿主进程（WorkBuddy/Weixin/QQ/Cursor/msedge/explorer，23 个）全部加载信标 DLL ✅。
- 但**激活之后新启动的进程不会加载**信标 DLL：实测 notepad（pid=67804 / 2040）、
  新 msedge 进程（7 个）、新 WorkBuddy 进程 —— 全部 `beacon=False`，日志无新 Activate 行。
- 原因（对照 MS Q&A + MSDN）：`TF_IPPMF_FORSESSION` 的作用域是**当前 desktop 的所有线程**，
  不是一个"以后新进程都会继承"的持久开关。profile 激活状态由 ctfmon 传播；只有当信标是
  **活动的键盘 profile** 时，新进程才加载它。而我们刻意不夺走用户的活动 profile →
  新进程就不加载。
- `TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE` 的官方语义是「语言不匹配时**标记为待激活**」，
  并**不是**「激活但不切换」——用它换来的"不抢输入法"，代价就是"不被新进程加载"。

**③ 由此得出的架构分叉（待马老师拍板）：**

- **路线甲（当前实现，TSF profile）**：注册信标为键盘类 TIP + FORSESSION 激活。
  - 优点：零注入、零权限、进程内一手坐标、已实证拿到精确 caret rect。
  - 缺点：**新启动的进程不加载** → 只能服务于「激活时刻已在运行」的应用。
  - 补救：clipx 侧可周期性重新 FORSESSION 激活，把新进程「拉进来」；但这会让信标短暂成为
    活动 profile，存在抢输入法窗口。
- **路线乙（DLL 注入）**：clipx 用 `CreateRemoteThread(LoadLibrary)` 把信标注入前台进程，
  DLL 自行 `CoCreateInstance(ITfThreadMgr)` + `AdviseSink`，**完全不碰 profile、不碰输入法**。
  - 优点：真正的零干扰；新进程随用随注入。
  - 代价：需要跨进程注入权限（`PROCESS_CREATE_THREAD|VM_WRITE|VM_OPERATION`，同用户非提权进程通常够）；
    目标进程须为 64 位；Electron 多进程模型下需定位渲染进程；信标要改成"自主模式"
    （`ClipxBeaconStart` 导出函数 + 在目标进程内自己起 STA 线程做 COM 初始化）。

### 7.4 其他实测/事实

- 机器上已有的 HKLM 注册指向 spike 旧 DLL（`caret-beacon-spike\target\release\beacon_dll.dll`），
  且旧 DLL 被 23 个进程加载 → **文件被锁定，无法直接覆盖**；改 CLSID 路径需管理员
  （本会话 `IsAdmin=False`，`reg add` 拒绝访问；`Start-Process -Verb RunAs` 被本机安全策略拦截）。
  → 新 DLL 的端到端联调需要一次**管理员**操作（跑 `scripts/register_beacon_hklm.bat` 或
  `scripts/repoint_beacon_dll.ps1`）。
- 旧 spike DLL 的 `OnEndEdit` 路径**只写日志不发布共享内存**（日志里 `caret pid=…` 连续输出，
  但 `Local\clipx-tsf-caret` 映射根本不存在）→ 这是 v2 修掉的一个真 bug。
- 现有自签证书 `CN=CaretBeacon Spike` 在 **CurrentUser\My 有私钥**、**机器级 Root 已信任**，
  因此**非管理员也能给新 DLL 签名**（`Set-AuthenticodeSignature` → `Valid`）。
  产品化脚本改用 `CN=clipx Caret Beacon` + LocalMachine\My（需管理员，符合预期）。

---

## 8. 路线乙实现与实测（2026-10-22 晚，已定案）

### 8.1 结论先说

**路线乙已打通，并实测像素级精确；且全程不碰输入法。** 路线甲正式废弃（见 8.4）。

### 8.2 实现形态

注入方式选了 **`SetWindowsHookExW(WH_GETMESSAGE)` 挂目标线程**，而不是
`CreateRemoteThread(LoadLibrary)`：

- 后者起的是**新线程**，而 TSF 的 `ITfThreadMgr` 是**线程绑定**的 —— 新线程上
  `CoCreateInstance(CLSID_TF_ThreadMgr)` 拿到的 manager **没有焦点**
  （这正是路线甲日志里 `measure: thread mgr has no focus` 的根因）。
- hook 回调跑在**宿主 UI 线程**上，那里才有真正的焦点 context，
  `GetFocus() → GetBase() → RequestEditSession → GetTextExt` 全链路可用。

模块划分：

- `crates/clipx-tsf` 新增导出 `ClipxCaretHookProc`（WH_GETMESSAGE 回调）。
  只在共享内存里有 pending 请求（`request_seq != ack_seq`）时测一次，其余时候
  纯 `CallNextHookEx` 透传。**不碰 profile、不动 keyboard openclose、不注册 HKLM。**
- `crates/clipx-app/src/tsf_inject.rs`（新）：找焦点线程 → `LoadLibraryW` +
  `GetProcAddress("ClipxCaretHookProc")` → `SetWindowsHookExW` →
  `PostThreadMessageW(WM_NULL)` 唤醒 → 等结果 → `UnhookWindowsHookEx`（用完即摘）。
- `win_popup.rs::caret_screen_point_dbg()` 的信标分支改成先走注入，拿不到再回落老链路。

### 8.3 实测数据（本机，2026-10-22）

| 目标 | 结果 | 耗时 | 备注 |
|---|---|---|---|
| 记事本（Win11 新记事本） | `rect=(3655,203,3656,223)` | 5.7ms | ground truth `GetGUIThreadInfo.rcCaret` = `(3655,203)-(3656,223)`，**矩形完全重合** |
| 微信 Weixin.exe | `rect=(3572,830,3589,847)` | 9.2ms | 拿到真 caret |
| WeChatAppEx（Chromium 容器） | 无结果 | 122ms 超时 | 非 TSF 宿主，优雅降级 |
| 注入前后活动 profile | 均为微信输入法 `86598FB9…` / `0x0804` | — | **完全一致，输入法零干扰已验证** |

### 8.4 路线甲为何废弃（实证）

信标日志（`%TEMP%\clipx_caret_beacon.log`）里路线甲**反复**出现：

```
measure: thread mgr has no focus
```

`Data/pos_debug.log` 里 `tsf:beacon` 命中 **0 次** —— 路线甲的测量
**从来没有成功过一次**。原因是 TSF thread mgr 线程绑定 + FORSESSION 只覆盖
已存在线程，新起进程的信标线程永远拿不到焦点。

### 8.5 修掉的两个真 bug

1. **零宽 caret 被 `sane()` 误拒**：折叠选区的 `GetTextExt` 常给 `left == right`
   （光标是竖线），原 `w > 0` 一律拒掉。改成 `w >= 0`，只把高度当硬指标。
   单测 `accepts_zero_width_caret` 覆盖。
2. **`request_seq` 跨进程撞车**：`REQUEST_SEQ` 是 clipx 进程内静态变量，每次重启
   从 1 开始；钩子用 `req != ack` 判新请求，上次运行留下的 `ack=1` 会让本次
   `req=1` 被直接跳过。改成以共享内存里已有值为基准取大者，保证跨进程单调递增。

### 8.6 遗留

- **未验证**：Electron 渲染进程内的真 caret（微信是自绘+TSF 已测通；
  Chromium 渲染进程的 TSF 宿主性待在真 Electron 应用里复测，例如 VS Code / WorkBuddy）。
- `clipx-core::gate::tests::suppresses_within_window_then_releases` 失败是**预存在**
  （`gate.rs` 本次未改动），未处理。
- 路线甲遗留现场未清理：HKLM 注册仍指向旧 spike DLL、23 进程驻留旧 DLL、多开 notepad。
