# 微信「UIA 元素框兜底」思路对 WorkBuddy（Electron）的适用性研究

日期：2026-09-21 起研，2026-09-22 中午闭环 ｜ 状态：**结论实锤，field-box 兜底已落地**

## 研究问题

微信 4.x 修复（commit 6aa08a3）的核心思路是「三层定位」：真 caret（TextPattern selection rects）→ 失焦/空框时 UIA **元素级 BoundingBox** 兜底 → 几何猜。同一思路是否适用于 WorkBuddy（Electron 应用）？

## 结论（TL;DR，2026-09-22 v26-v30 修订版）

1. **思路完全适用，且不需要 IA2——原生 UIA 路线直接可用。** 之前「UIA 树 Edit=0」是 **.NET System.Windows.Automation 的假象**：同一前台时刻，pywinauto（原生 UIA 客户端 UiaCore）能稳定看到 `Edit rect=(3177,874 762x64)`（聊天输入框，宽度随窗自适应）+ TextPattern。clipx 的 windows-rs UIA 代码走的就是原生 API，**方向从来没错过**。
2. **WorkBuddy 的 TextPattern selection 与微信 4.x 空框完全同构**：`GetSelection()` 返回 1 个 collapsed range，`GetBoundingRectangles()` 是**空数组**（v30 实锤 selections=1 rects=[]）——文字级没矩形，与微信 collapsed selection 一致；而**元素级 BBox 可信**（空框也给、跟随窗宽/布局自适应）。
3. **IA2（IAccessibleText）out-of-proc 全线不可用**：全树 680 节点 QI(IID_IAccessibleText) 零命中（v26），Edge（Chromium 同代）对照组同样零命中（v28，765 节点）——Chromium 140 的 out-of-proc IA2Text 已名存实亡（官方博客承诺「MSAA/IA2 客户端支持不变」与实测不符，MSAA 树活着但 IA2Text 接口不下放）。IA2 手写 vtable 方案**废弃**。
4. 落地（已完成，见下）：`uia_composer_box` 尾部新增 **field-box 兜底**——TextPattern selection rects 为空时直接用 focused 元素 BBox 作定位框（`is_usable_composer` 过滤），branch 字串 `field-box`。

## 证据链

### 关键对照（全部被动采样，不抢焦点）

| 探针 | 客户端 API | 窗口状态 | 结果 |
|---|---|---|---|
| v16/v23（.NET SWA） | UIA wrapper | WB 前台 | Edit=0 Doc=0（**假象，见 v27**） |
| v21/v22（comtypes） | MSAA | WB 前台 | 96 节点壳 / 1279 行全树 dump，web 层齐备 |
| v26（comtypes） | MSAA→IA2Text QI | WB 前台 | 680 节点 **IA2Text 零命中** ×8 轮 |
| v28（comtypes，对照组） | MSAA→IA2Text QI | Edge 后台 | 765 节点 **同样零命中** → QI 代码无 bug，平台共性 |
| v27（pywinauto） | **原生 UIA** | WB 前台 | **Edit 可见** rect=(3508,874 928x64)，宽度随窗变化 |
| v30（pywinauto） | 原生 UIA+TextPattern | WB 前台 | selections=1，**rects=[]**（collapsed，微信同构） |

### 教训

- **.NET System.Windows.Automation 不是 UIA 语义的裁决者**——它的树/缓存机制会漏掉 Electron web 子树。做 UIA 可行性判断必须用原生 API（UiaCore）。
- Chromium 官方「继续支持 IA2」的承诺范围 ≠ out-of-proc IA2Text 可用性。以探针实测为准。

### 数据陷阱（实现时仍须遵守）

1. **off-screen 虚拟内容**：MSAA 树上大量 TEXT(edit) 在 y=-2400 级别（虚拟滚动/隐藏 tab）。`is_usable_composer` 的 fg 比例过滤已覆盖。
2. **焦点不在输入框时**（浏览态，focused=DOCUMENT）：元素框过大被 `is_usable_composer` 拒 → 正确落到 no-composer 几何猜（用户焦点不在输入框，几何猜合理）。

## 已落地的修复（2026-09-22）

`win_popup.rs::uia_composer_box` 尾部、`done(None,"no-composer")` 前插入：

```rust
// TextPattern selection 是 collapsed range 时 GetBoundingRectangles
// 返回空数组（v30 实锤：WorkBuddy 空框 selections=1 rects=[]，微信同构）。
// 元素级 BBox 仍可信：空框也给、跟随窗口/布局自适应（v27）。
if let Some(b) = bounds(&el) {
    if is_usable_composer(Some(b), fg) {
        return done(Some(box_of(b)), "field-box ...");
    }
}
```

覆盖两种此前 miss 的情形：①有 TextPattern 但 collapsed rects 空；②TextPattern 完全 miss 但 focused 元素框可用。pos_debug 的 45 次 `no-composer`（纯几何猜偏差主力）预期被大幅吞掉；75 次 `parent`（父链大控件）不受影响。

## 遗留

- field-box 生效率需真机观察 pos_debug `branch=field-box` 分布（实例已带新代码重启）。
- `no-composer` 残余可能来自 150ms 内 ax 树未建好（Chromium 惰性构建）；如仍高频，再考虑 150ms→300ms 的延迟取舍。
- IA2 in-process（注入 DLL）理论可行但成本不成比例，不考虑。

## 探针资产（.workbuddy/tmp/）

- **v22**：MSAA 全树 dump 参考实现（被动等前台、全节点 role/state/rect）
- **v27/v30**：pywinauto 原生 UIA 探针（本研究的裁决者）
- v26/v28：IA2Text QI 扫描（Edge 对照方法论）
- venv（`~/.workbuddy/binaries/python/envs/default`）：comtypes + pywin32 + pywinauto

## 运维发现（已入记忆）

- **PowerShell 5.1 + UTF-8 无 BOM 脚本含中文 = 整个脚本不执行**（GBK 误解析、解析阶段失败）。写完用 Python 补 `EF BB BF`。
- comtypes VARIANT 子节点解引用用 `v.value`；`comtypes.VARIANT` 不存在，须 `from comtypes.automation import VARIANT`。
- 外部进程 SetForegroundWindow 会被前台锁拒绝 → **被动轮询前台**才是正道。
- PowerShell 动态调 COM（IDispatch 链）会 0xC0000005 崩 → MSAA/UIA 探针一律 Python comtypes。

## 2026-09-22 晚更正：WorkBuddy 放弃 caret2 横向跟光标

`b94d7dc` 把 caret2 的插入框改成以光标为对称心的 ±380 假框，用户实测「更不好用了，
不如恢复输入框上方居中」。复盘出两个问题，**结论：WorkBuddy 不走 caret2**。

### 根因一：假框中心 = 光标 → 弹窗横着飘

`popup_rect_on_box` 的 `x = bl + (bw-pw)/2` 是「居中于传入 box」。把 box 做成
「以光标为心的 ±380」后，居中结果就等价于「弹窗中心跟随光标」。用户要的是
**居中于输入框**，不是跟随光标。

### 根因二：跨屏错屏（更严重）

```rust
let bx0 = (pt.0 - 380).max(fg.0 + 8);   // 用「前台窗左边界」做 clamp
```

`fg.0` 是**跨屏的物理坐标**（WorkBuddy 主窗横跨主/副屏，`fgrect=(2867,-14,4813,1044)`），
而 `popup_rect_on_box` 的 `work` 是按**锚点**取的屏。两者坐标系不一致时弹窗整体被
clamp 到错误屏。`pos_debug.log` 实锤：

```
show ... branch=workbuddy:caret2 anchor=(2875,884) work=(0,0,2880,1824) rect=(1976,0,904x1184)
```

锚点在副屏，rect 却落到主屏右端（1976 是主屏 2880 - 904）。

### 改法

1. `uia_composer_box(fg, prefer_caret: bool)`：WorkBuddy 传 `false`，**跳过 TextPattern2**，
   只用 parent/field-box 链拿真实输入框 BBox（拿不到再几何估 `estimate_large_input_box`）。
   弹窗 `popup_rect_on_box` 水平居中于该框、垂直放其上/下 —— 与 WPF 语义一致。
   caret2（`prefer_caret=true`）仍保留给微信等未特判应用。
2. `popup_rect` 的 `PLACE_ON_BOX` 分支**按输入框中心重新取屏与 DPI**
   （`monitor_work_and_dpi(cx, cy)`），不再沿用锚点屏 —— 修所有走 `popup_rect_on_box`
   路径的跨屏错屏（含微信）。

### 教训

- **别把「光标位置」当「弹窗中心」的代理**：两者的合理基准不同（光标在行内，弹窗要居中于框）。
- **clamp 用的坐标系必须与最终定位用的坐标系同源**：混用「跨屏绝对坐标」与「某屏坐标」
  是静默错屏的温床。
