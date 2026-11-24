# 搜索融合排序（u64 分段分）

对应 listary 计划表 #1（`CombinedScore`）+ §5.3（app 亲和）。
落点：`clipx-app/src/logic.rs::rank_score`（Rust 侧精排）；
`clipx-store` 只供数（`use_count`/`last_used_ms` 进 `EntryMeta`）与 SQL 预排。

## 位布局（高位优先，数值越大越靠前）

| 位 | 段 | 含义 |
|---|---|---|
| 63 | ext | 短语/外部行（`ItemId::Ext`）。沿用旧 "+80 压倒一切"。 |
| 62 | pinned | 置顶（沿用旧 "+40"）。 |
| 59–61 | match | 5 精确相等 / 4 字面前缀 / 3 拼音起始 / 2 字面包含 / 1 其他命中 / 0 无信号（空查询全 0）。 |
| 58 | affinity | `source_app == 当前前台 exe`（见下）。 |
| 38–57 | freq | `min(use_count, 2^20-1)`，饱和截断（100 万次封顶，再多不加分）。 |
| 0–37 | recency | `min(last_used_ms/1000, 2^38-1)`，秒级（38 位约够 8700 年）。 |

match 相对顺序沿用旧 f64 分：拼音起始（旧 18）> 字面包含（旧 10）。
精确/前缀/包含走小写字面比较；拼音走 `pinyin_hit_span` 起始位置；
深搜（full_text/OCR）与 FTS 命中落到 1（"在预览之外命中"）。

## 空查询（默认列表）：纯时间序，不走融合

空查询（默认列表）**不用融合排序**：`pinned DESC, created_ms DESC`（SQL 原序，
对齐 WPF）。理由：默认列表是"最近用的东西"，时间序即心智；
频率只用在"有明确意图"处：非空搜索（本融合）、短语组内排序、
filejump 全盘补充组（`confirm_counts` 降序）。
短语行仍排在历史之后（空查询先排历史再拼短语）。

## affinity（§5.3）

- 两边同一归一化：采集侧 `policy::foreground_app` 与查询侧
  `win_popup::foreground_exe_base` 都是"小写 stem、无 `.exe`"（如 `notepad`）。
- 任一边为空串即无加成；自家弹窗在前台时按无前台处理（不自己给自己加分）。
- 与 `source_filter`（按应用硬过滤）正交：过滤是"只看"，亲和是"加权"。

## SQL 预排与 Rust 精排的分工

- SQL（`handle_search`）：`pinned DESC, use_count DESC, created_ms DESC`，
  保证候选集（`limit` 窗口，默认 ≤2000）里高频行不被"最新优先"挤出去。
- Rust（`rank_results`，Schwartzian 小写缓存）：u64 精排 + 稳定排序。
- `use_count` 排序是 filesort，零命中全表扫场景已在基线内（20k 下 15–25ms）。

## 与 WPF 的关系（PRD §8 第 7 条）

WPF 版按时间 + 置顶排序，无 freq/affinity 概念。本排序是**有意超越**：
"越用越准"（freq）、"在哪用哪准"（affinity）。关掉置顶开关只停重排，
计数照记（`MarkUsed`），freq 维度不受影响。

## 例子

查询 `entry`，前台 `notepad`：

| 行 | tier | affinity | freq | 结论 |
|---|---|---|---|---|
| "entry zz"（msedge，0 次） | 4 前缀 | 0 | 0 | 第一：match 压倒一切 |
| "yy entry"（notepad，0 次） | 2 包含 | 1 | 0 | 第二：同档内 affinity 胜出 |
| "xx entry"（msedge，100 次） | 2 包含 | 0 | 100 | 第三：affinity 高一位，freq 再往后 |
