//! 文件夹跳转 Picker（M5d）：对齐 WPF `FileDialogJumpPickerWindow` + `FileJumpHotkey`。
//!
//! - 全局 `Ctrl+G`：有对话框 → 采集候选并弹列表；无对话框 → 收藏/最近/管理器列表，Enter 在资源管理器打开
//! - 对话框前台自动弹出（`filejump_auto_popup`，400ms 轮询，WPF 前台事件的轻量版）
//! - 自动跳转最佳路径（`filejump_auto_jump`，默认关）
//! - 采集与导航全在后台线程（TC 剪贴板借道 + dopusrt + 注入都阻塞），gen 丢弃过期结果
//! - 键盘复用主弹窗钩子通道（VISIBLE 共享位）：↑↓←→/PgUp/PgDn/Home/End 移动，
//!   Enter 跳转，Esc 清搜索/关闭，Del 移除收藏/最近，Menu/Ctrl+P 收藏切换

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

static FOLLOW_MOUSE: AtomicBool = AtomicBool::new(false);

pub fn set_follow_mouse(v: bool) {
    FOLLOW_MOUSE.store(v, Ordering::SeqCst);
}

use clipx_filejump::{Candidate, CandidateSource, DialogIntent, DialogKind};
use slint::ComponentHandle;

use crate::logic::AppEvt;
use crate::settings::{self, Settings};

/// 内卡片宽；窗口总宽 = 此值 + 两侧阴影 16×2 = 400，比 WPF 500 / 剪贴板 452 更窄。
pub const FJ_W: f32 = 368.0;
const FJ_CHROME: f32 = 16.0;
const FJ_ROW_H: f32 = 36.0;
const FJ_UI_CHROME: f32 = 108.0; // header 40 + search 36 + footer 32
const FJ_MIN_H: f32 = 168.0;
const FJ_MAX_H: f32 = 500.0;
const PAGE: usize = 8;

#[derive(Default)]
pub struct FjState {
    pub visible: bool,
    pub dialog: isize,
    pub kind: Option<DialogKind>,
    /// 对话框意图（挑文件还是挑文件夹）：决定检索是否收文件、确认时定位文件还是跳文件夹。
    pub intent: DialogIntent,
    /// 无对话框时的全局模式：Enter 在资源管理器打开而非导航。
    pub global_mode: bool,
    pub query: String,
    pub items: Vec<Candidate>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub first_visible: usize,
    pub gen: u64,
    pub pending_auto_jump: bool,
    pub navigating: bool,
    pub hint: String,
    pub shown_at: Option<Instant>,
    /// 跟随线程 kill event（HANDLE 存 isize，0=无）；hide 时 signal，线程负责 unhook + 关闭。
    pub follow_kill: isize,
    /// 兜底 tick 比较基准：对话框上次矩形（物理像素）。
    /// WinEvent 是实时主力；个别不发 LOCATIONCHANGE 的宿主靠 200ms tick 补。
    pub last_dlg_rect: Option<(i32, i32, i32, i32)>,
    /// Tab 仅收藏
    pub fav_only: bool,
    /// 二次 Ctrl+G 直跳
    pub last_hotkey_at: Option<Instant>,
    /// auto_sync：曾离开对话框
    pub left_dialog: bool,
    /// 离开对话框时去往的顶层窗口（回来时按它自己的路径联动，多个资源管理器更准）
    pub left_to: isize,
    /// 用户最近一次**手动**跳转（点列表 / Enter）的时刻。auto_sync 在此后
    /// 1.5s 内不得再自动跳一次——否则刚选的目录会被"切回同步"覆盖
    /// （实证 bug：点列表时焦点短暂算离开，回来立刻触发同步跳回旧路径）。
    pub last_manual_at: Option<Instant>,
    /// 对话框当前文件夹（采集线程回读，查询时把子文件夹插到其后面）。
    pub current_folder: Option<String>,
    /// 上次回读对话框当前目录的时刻。打字不再每键注入探测——注入要
    /// CreateRemoteThread 到宿主进程里（会打断它），改由 poll tick 低频刷新。
    pub folder_read_at: Option<Instant>,
}

pub fn source_label(s: CandidateSource) -> &'static str {
    match s {
        CandidateSource::Manager => "管理器",
        CandidateSource::Favorite => "收藏",
        CandidateSource::Recent => "常用",
        CandidateSource::Last => "上次",
        CandidateSource::Everything => "everything",
        CandidateSource::Local => "本地",
        CandidateSource::Current => "当前",
    }
}

fn source_kind(s: CandidateSource) -> i32 {
    match s {
        CandidateSource::Manager => 0,
        CandidateSource::Favorite => 1,
        CandidateSource::Recent => 2,
        CandidateSource::Last => 3,
        CandidateSource::Everything => 4,
        CandidateSource::Current => 5,
        CandidateSource::Local => 6,
    }
}

/// 子串 / 拼音 / 空格分词 AND（路径或别名任一命中即可）。
/// 过滤上限：防止列表爆炸拖慢 UI 推送。
const MAX_FILTERED: usize = 200;
pub fn filter_items(items: &[Candidate], query: &str) -> Vec<usize> {
    let q = query.trim();
    if q.is_empty() {
        return (0..items.len().min(MAX_FILTERED)).collect();
    }
    let mut out = Vec::new();
    for (i, c) in items.iter().enumerate() {
        if out.len() >= MAX_FILTERED {
            break;
        }
        let matched = q.split_whitespace().all(|tok| {
            clipx_core::pinyin::text_matches_query(&c.path, tok)
                || c.alias
                    .as_deref()
                    .is_some_and(|a| clipx_core::pinyin::text_matches_query(a, tok))
        });
        if matched {
            out.push(i);
        }
    }
    out
}

fn fav_candidates(favs: &[crate::settings::FolderFavorite]) -> Vec<Candidate> {
    favs.iter()
        .filter_map(|f| {
            let norm = clipx_filejump::collectors::normalize_path(f.path())?;
            if !std::path::Path::new(&norm).is_dir() {
                return None;
            }
            let alias = if f.phrase().is_empty() {
                None
            } else {
                Some(f.phrase().to_string())
            };
            Some(Candidate { path: norm, alias, source: CandidateSource::Favorite, is_file: false })
        })
        .collect()
}

fn recent_candidates(recent: &[String]) -> Vec<Candidate> {
    recent.iter()
        .filter_map(|p| {
            let norm = clipx_filejump::collectors::normalize_path(p)?;
            if !std::path::Path::new(&norm).is_dir() {
                return None;
            }
            Some(Candidate { path: norm, alias: None, source: CandidateSource::Recent, is_file: false })
        })
        .collect()
}

pub(crate) fn fj_log(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("clipx_inject.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let ts = clipx_core::time::hms_now();
        let _ = writeln!(f, "[fj {ts}] {msg}");
    }
}

/// 路径拆成「名称 + 父目录」（尾部分隔符先归一）。
fn split_display_name(path: &str) -> (String, String) {
    let s = path.trim().trim_end_matches(['\\', '/']);
    if s.is_empty() {
        return (path.trim().to_string(), String::new());
    }
    // 盘符根（C: / C:\）不拆。
    if s.len() <= 2 && s.ends_with(':') {
        return (path.trim().to_string(), String::new());
    }
    match s.rfind(['\\', '/']) {
        Some(i) if i >= 2 => {
            // C:\Users → 父目录补回盘符反斜杠（C:\Users 的 rfind 在 2，前缀只剩 "C:"）。
            let parent = if i == 2 && s.as_bytes()[1] == b':' {
                format!("{}:\\", &s[..2])
            } else {
                s[..i].to_string()
            };
            (s[i + 1..].to_string(), parent)
        }
        Some(_) => (path.trim().to_string(), String::new()),
        None => (s.to_string(), String::new()),
    }
}

/// 保留尾部 `keep` 个字符，前面补省略号（用于命中段之前的上下文）。
fn tail_ellipsis(s: &str, keep: usize) -> String {
    let n = s.chars().count();
    if n <= keep {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - keep).collect();
    format!("…{tail}")
}

/// 保留头部 `keep` 个字符，后面补省略号（用于命中段之后的上下文）。
fn head_ellipsis(s: &str, keep: usize) -> String {
    let n = s.chars().count();
    if n <= keep {
        return s.to_string();
    }
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

fn current_folder_candidate(dialog: isize) -> Option<Candidate> {
    if dialog == 0 {
        return None;
    }
    // 注入优先（最可靠），失败走 UIA ValuePattern 回退（对齐 WPF TryReadCurrentFolder）。
    let raw = clipx_filejump::inject::read_current_folder(dialog)
        .ok()
        .or_else(|| {
            fj_log("current_folder: inject failed, trying UIA");
            crate::win_popup::uia_dialog_folder(dialog)
        });
    match &raw {
        Some(r) => fj_log(&format!("current_folder: got {r}")),
        None => fj_log("current_folder: both inject+UIA failed"),
    }
    let raw = raw?;
    let norm = clipx_filejump::collectors::normalize_path(&raw)?;
    if !Path::new(&norm).is_dir() {
        return None;
    }
    Some(Candidate {
        path: norm,
        alias: None,
        source: CandidateSource::Current,
        is_file: false,
    })
}

/// 当前文件夹下一层子项（查询时插在「当前」后面、全盘 everything 前面）。
///
/// `include_files`：对话框意图是**选文件**时连文件一起搜；选文件夹时只搜子目录。
fn list_child_items(folder: &str, query: &str, max: usize, include_files: bool) -> Vec<Candidate> {
    let q = query.trim();
    if q.is_empty() || max == 0 {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir && !include_files {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        if !clipx_core::pinyin::text_matches_query(&name, q) {
            continue;
        }
        let path = ent.path().to_string_lossy().replace('/', "\\");
        out.push(Candidate {
            path,
            alias: None,
            source: CandidateSource::Current,
            is_file: !is_dir,
        });
    }
    // 文件夹优先，再按路径字典序（对齐原 list_child_folders 排序习惯）。
    out.sort_by(|a, b| {
        b.is_folder()
            .cmp(&a.is_folder())
            .then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
    });
    out.truncate(max);
    out
}

/// Everything 熔断：探测/查询失败后这段时间内直接跳过（毫秒 epoch）。
/// Everything IPC 不回复时，单次查询要白等 探测(250+180+180) + 800ms ≈ 1.4s，
/// 每个键都来一遍就是「打字卡好几秒」；熔断后恢复即时。
static EV_BREAKER_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// 超时是「慢」不是「死」：30s 熔断会让用户以为全盘检索消失。5s 足够躲连击。
const EV_BREAKER_MS: u64 = 5_000;

fn ev_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ev_tripped() -> bool {
    ev_now_ms() < EV_BREAKER_UNTIL.load(std::sync::atomic::Ordering::Relaxed)
}

/// 外部索引查询。返回 `(命中, 引擎是否可用)`。
///
/// `engine_ok == false` 表示**没有任何索引结果可信**（服务没跑 / 熔断中）：
/// 调用方据此决定是否退到 [`deep_local_items`]。
fn index_items(
    query: &str,
    max: usize,
    include_files: bool,
    engine: clipx_everything::Engine,
) -> (Vec<Candidate>, bool) {
    let q = query.trim();
    if q.is_empty() || max == 0 {
        return (Vec::new(), false);
    }
    if ev_tripped() {
        return (Vec::new(), false);
    }
    // Everything IPC 实测单查 40-55ms（probe 独立进程 + 进程内一致）。
    // 400ms = 8 倍余量：足够吞掉偶发的大结果集抖动，又不会在真的挂掉时
    // 让用户多等一截。超时即熔断 5s，最坏每 5s 只赔 400ms。
    let timeout = Duration::from_millis(400);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let push_from = |res: clipx_everything::QueryResults,
                     seen: &mut std::collections::HashSet<String>,
                     out: &mut Vec<Candidate>| {
        for it in res.items {
            let is_dir = it.is_folder;
            if !include_files && !is_dir {
                continue;
            }
            // 注意：这里**禁止**做 Path::is_dir()/is_file() 文件系统校验——
            // Everything 结果含网络盘/已断开映射盘路径时，is_dir() 会阻塞到
            // SMB 超时（单条数秒），几十条 = 打字卡几十秒（2026-10-23 实证）。
            // 直接信任 it.is_folder；索引过期的代价只是点击后跳转失败（有回退）。
            if !seen.insert(it.full_path.to_lowercase()) {
                continue;
            }
            out.push(Candidate {
                path: it.full_path,
                alias: None,
                source: CandidateSource::Everything,
                is_file: !is_dir,
            });
            if out.len() >= max {
                break;
            }
        }
    };
    // 表达式按意图选，**一次查询搞定**（旧实现先 `folder:"q"` 再补一枪裸词，
    // 两次 500ms 超时叠加就是 1s+）：
    // - 选文件夹 + Everything → `folder:"q"`（folder: 只回文件夹，files=0）
    // - 选文件   → 裸词（文件+文件夹都回，`include_files` 决定要不要丢文件）
    // - FindX    → 恒裸词：它的私有协议只认关键词，`folder:` 会被当普通文本。
    let expr = if include_files || engine == clipx_everything::Engine::FindX {
        q.to_string()
    } else {
        clipx_everything::search::build_folder_search(q)
    };
    match clipx_everything::query_engine(engine, &expr, 50, timeout) {
        Ok(res) => {
            push_from(res, &mut seen, &mut out);
            (out, true)
        }
        Err(e) => {
            // 服务不通就别再补第二枪（每枪 1s+），直接熔断。
            EV_BREAKER_UNTIL.store(
                ev_now_ms() + EV_BREAKER_MS,
                std::sync::atomic::Ordering::Relaxed,
            );
            fj_log(&format!(
                "ev: engine={} err={e:?} → breaker {EV_BREAKER_MS}ms",
                engine.as_setting()
            ));
            (out, false)
        }
    }
}

/// 内置目录递归：**没有任何索引服务可用时的退路**。
///
/// 从对话框当前目录向下 BFS，深度上限 3、访问节点上限 4000、耗时预算 600ms，
/// 命中即收（最多 `max` 条）。跳过符号链接与几个必然巨大的目录，避免一次
/// 搜索把 IO 占满。命中判定复用 `clipx_core::pinyin::text_matches_query`，
/// 所以 `mct` 也能匹配「马春天」——和当前目录一层检索口径一致。
fn deep_local_items(
    folder: &str,
    query: &str,
    max: usize,
    include_files: bool,
) -> Vec<Candidate> {
    use std::time::Instant;
    const MAX_DEPTH: usize = 3;
    const MAX_VISIT: usize = 4000;
    const BUDGET: Duration = Duration::from_millis(300);
    /// 这些目录进去只会拖时间：版本库内部、依赖树、回收站、系统卷影。
    const SKIP: &[&str] = &[
        ".git",
        ".svn",
        ".hg",
        "node_modules",
        "target",
        "$recycle.bin",
        "system volume information",
    ];
    let q = query.trim();
    if q.is_empty() || max == 0 {
        return Vec::new();
    }
    let t0 = Instant::now();
    let mut out = Vec::new();
    let mut visited = 0usize;
    let mut queue: std::collections::VecDeque<(String, usize)> =
        std::collections::VecDeque::new();
    queue.push_back((folder.to_string(), 0));
    'walk: while let Some((dir, depth)) = queue.pop_front() {
        if out.len() >= max || visited >= MAX_VISIT || t0.elapsed() > BUDGET {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            visited += 1;
            if visited > MAX_VISIT || t0.elapsed() > BUDGET {
                break 'walk;
            }
            let Ok(ft) = ent.file_type() else { continue };
            // 符号链接/联接点不追：防环，也防跨盘。
            if ft.is_symlink() {
                continue;
            }
            let is_dir = ft.is_dir();
            let name = ent.file_name().to_string_lossy().into_owned();
            if is_dir && depth < MAX_DEPTH && !SKIP.iter().any(|s| name.eq_ignore_ascii_case(s)) {
                queue.push_back((ent.path().to_string_lossy().into_owned(), depth + 1));
            }
            if !is_dir && !include_files {
                continue;
            }
            if !clipx_core::pinyin::text_matches_query(&name, q) {
                continue;
            }
            out.push(Candidate {
                path: ent.path().to_string_lossy().replace('/', "\\"),
                alias: None,
                source: CandidateSource::Local,
                is_file: !is_dir,
            });
            if out.len() >= max {
                break 'walk;
            }
        }
    }
    out
}

// ================= 后台采集 =================

pub struct CollectReq {
    pub gen: u64,
    pub query: String,
    pub favorites: Vec<crate::settings::FolderFavorite>,
    pub recent: Vec<String>,
    pub everything_enabled: bool,
    /// 全盘检索用哪个索引引擎（Everything / FindX）。
    pub engine: clipx_everything::Engine,
    pub dialog: isize,
    /// 选文件对话框 → true（子项/Everything 连文件一起收）。
    pub include_files: bool,
}

pub fn spawn_collect(
    tx: Sender<AppEvt>,
    gate: clipx_core::ClipboardGate,
    req: CollectReq,
) {
    std::thread::Builder::new()
        .name("clipx-fj-collect".into())
        .spawn(move || {
            let t0 = std::time::Instant::now();
            let current = current_folder_candidate(req.dialog);
            let t_dir = t0.elapsed();
            let children = current
                .as_ref()
                .map(|c| list_child_items(&c.path, &req.query, 20, req.include_files))
                .unwrap_or_default();
            let t_children = t0.elapsed();
            let collected = clipx_filejump::collectors::collect(Some(&gate));
            let t_collect = t0.elapsed();
            let cur_path = current.as_ref().map(|c| c.path.clone());
            let mut groups = vec![
                current.into_iter().collect::<Vec<_>>(),
                children,
                collected,
                fav_candidates(&req.favorites),
                recent_candidates(&req.recent),
            ];
            if req.everything_enabled && !req.query.trim().is_empty() {
                let (ev, engine_ok) = index_items(&req.query, 20, req.include_files, req.engine);
                groups.push(ev);
                // 索引服务不可用 → 内置递归兜底（否则「都没装」时全盘一片空白）。
                if !engine_ok {
                    if let Some(f) = cur_path.as_deref() {
                        let deep = deep_local_items(f, &req.query, 20, req.include_files);
                        fj_log(&format!(
                            "collect: engine={} unavailable → local deep n={}",
                            req.engine.as_setting(),
                            deep.len()
                        ));
                        groups.push(deep);
                    }
                }
            }
            let n_children = groups.get(1).map(|g| g.len()).unwrap_or(0);
            let items = clipx_filejump::collectors::merge_candidates(groups);
            fj_log(&format!(
                "collect: dir={:?} children={:?}({}) collectors={:?} total={:?} n={}",
                t_dir,
                t_children,
                n_children,
                t_collect,
                t0.elapsed(),
                items.len()
            ));
            let _ = tx.send(AppEvt::FjCollected { gen: req.gen, items });
        })
        .ok();
}

/// 已派发的查询代际。每次派发即自增，worker 拿它判断手上的活是否已被顶掉。
static EV_QUERY_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 一次待执行的打字查询。
struct PendingEv {
    tx: Sender<AppEvt>,
    gen: u64,
    query: String,
    dialog: isize,
    everything_enabled: bool,
    include_files: bool,
    engine: clipx_everything::Engine,
    folder: Option<String>,
}

/// 打字查询的常驻工作线程槽位（只存**最新**一份请求）。
///
/// 旧实现是「每次按键 spawn 一个线程，进去先 `sleep(150ms)` 防抖」：
/// 150ms 是**白等**，实测日志里每个字符的 `dir_read` 恰好都是 150.0ms —
/// 真正的读目录只花 0.2ms。连打十个字 = 十个线程各自睡 150ms，
/// Everything 单查才 40-55ms，却被这段睡眠拖成「打字一顿一顿」。
///
/// 现在只有一个 worker：派发方把最新请求覆盖进槽位并唤醒，worker 干完手上
/// 的活立刻取走最新那份；被覆盖掉的中间态请求根本不会执行。既没有固定
/// 延迟，又天然合并连击，也不会堆线程。
static EV_SLOT: std::sync::Mutex<Option<PendingEv>> = std::sync::Mutex::new(None);
static EV_WAKE: std::sync::Condvar = std::sync::Condvar::new();
static EV_WORKER: std::sync::Once = std::sync::Once::new();

/// worker 主循环：取最新请求 → 执行 → 再取。
fn ev_worker_loop() {
    loop {
        let req = {
            let mut slot = match EV_SLOT.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            loop {
                if let Some(r) = slot.take() {
                    break r;
                }
                slot = match EV_WAKE.wait(slot) {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
            }
        };
        run_ev_query(req);
    }
}

/// 执行一次打字查询：本地子项先上屏，再补全盘索引。
fn run_ev_query(req: PendingEv) {
    let PendingEv {
        tx,
        gen,
        query,
        dialog,
        everything_enabled,
        include_files,
        engine,
        folder,
    } = req;
    let stale = || EV_QUERY_GEN.load(Ordering::SeqCst) != gen;

    // 排队期间已经被更新的按键顶掉 → 直接放弃，一个字节都不读。
    if stale() || query.trim().is_empty() {
        return;
    }
    let t0 = Instant::now();
    // 目录用缓存值；没有缓存（首次）才现读。
    let current_folder = match folder {
        Some(f) => Some(f),
        None => current_folder_candidate(dialog).map(|c| c.path),
    };
    let items = current_folder
        .as_deref()
        .map(|f| list_child_items(f, &query, 20, include_files))
        .unwrap_or_default();
    fj_log(&format!(
        "ev: q={query:?} local={:?} n_local={} folder={:?}",
        t0.elapsed(),
        items.len(),
        current_folder
    ));
    // ① 本地结果先上屏（当前目录命中立刻可见）。
    let _ = tx.send(AppEvt::FjEvResults { gen, items });
    if !everything_enabled || stale() {
        return;
    }
    // ② 全盘索引是补充，慢/挂都不该拖住本地结果。
    let ev = index_items(&query, 20, include_files, engine);
    fj_log(&format!(
        "ev: engine={} avail={} n_ev={} total={:?}",
        engine.as_setting(),
        ev.1,
        ev.0.len(),
        t0.elapsed()
    ));
    if ev.1 && !ev.0.is_empty() && !stale() {
        let _ = tx.send(AppEvt::FjEvResults { gen, items: ev.0 });
        return;
    }
    // ②b 没有索引服务（Everything/FindX 都没跑）→ 内置递归兜底。
    if !ev.1 {
        if let Some(f) = current_folder.as_deref() {
            let deep = deep_local_items(f, &query, 20, include_files);
            fj_log(&format!("ev: local-deep n={} total={:?}", deep.len(), t0.elapsed()));
            if !deep.is_empty() && !stale() {
                let _ = tx.send(AppEvt::FjEvResults { gen, items: deep });
            }
        }
    }
}

/// 打字后：当前文件夹子项 +（可选）Everything 全盘补充。
/// `include_files`：选文件对话框连文件一起搜。
pub fn spawn_ev_query(
    tx: Sender<AppEvt>,
    gen: u64,
    query: String,
    dialog: isize,
    everything_enabled: bool,
    include_files: bool,
    engine: clipx_everything::Engine,
    // 已知的对话框当前目录（poll tick 维护）。`None` 时才现读一次。
    // 为什么不在查询线程里每键都读：读目录要往宿主进程 CreateRemoteThread
    // 注入 DLL（会挂起目标线程），每敲一个字符注入一次会让文件对话框
    // 本身一顿一顿的（用户报「经常很卡顿」）。
    folder: Option<String>,
) {
    EV_QUERY_GEN.store(gen, Ordering::SeqCst);
    // 首次派发时拉起常驻 worker；之后只投递，不再创建线程。
    EV_WORKER.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("clipx-fj-ev".into())
            .spawn(ev_worker_loop);
    });
    let mut slot = match EV_SLOT.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    // 覆盖式投递：上一份没开工就被这份顶掉（连击天然合并）。
    *slot = Some(PendingEv {
        tx,
        gen,
        query,
        dialog,
        everything_enabled,
        include_files,
        engine,
        folder,
    });
    drop(slot);
    EV_WAKE.notify_one();
}

/// 后台回读对话框当前目录（poll tick 调用，~700ms 一次），结果发
/// [`AppEvt::FjFolder`]。绝不放在按键路径上。
pub fn spawn_folder_read(tx: Sender<AppEvt>, dialog: isize) {
    std::thread::Builder::new()
        .name("clipx-fj-folder".into())
        .spawn(move || {
            let folder = current_folder_candidate(dialog).map(|c| c.path);
            let _ = tx.send(AppEvt::FjFolder { dialog, folder });
        })
        .ok();
}

// ================= 后台导航 =================

pub fn spawn_navigate(
    tx: Sender<AppEvt>,
    dialog: isize,
    kind: DialogKind,
    path: String,
    allow_inject: bool,
    gen: u64,
    is_file: bool,
) {
    std::thread::Builder::new()
        .name("clipx-fj-nav".into())
        .spawn(move || {
            let ok = if is_file {
                // 选文件对话框：跳父目录 → UIA 在列表里真选中 → 兜底填文件名框。
                locate_file(dialog, kind, &path, allow_inject)
            } else {
                clipx_filejump::inject::navigate_to_folder(dialog, kind, &path, allow_inject)
                    .unwrap_or(false)
            };
            let _ = tx.send(AppEvt::FjNavigated {
                gen,
                path,
                ok,
                is_file,
            });
        })
        .ok();
}

/// 定位文件：父目录跳转 + 列表选中（UIA）/ 文件名框写入（兜底）。
fn locate_file(dialog: isize, kind: DialogKind, path: &str, allow_inject: bool) -> bool {
    let p = std::path::Path::new(path);
    let Some(parent) = p.parent() else {
        return false;
    };
    let parent_s = parent.to_string_lossy().to_string();
    let nav = clipx_filejump::inject::navigate_to_folder(dialog, kind, &parent_s, allow_inject)
        .unwrap_or(false);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return nav;
    }
    let t0 = std::time::Instant::now();
    let uia = crate::win_popup::uia_select_file(dialog, &name);
    // 文件名框也写上：列表选中 + 文件名一致，「打开」按钮必定可用。
    let set = clipx_filejump::inject::set_filename_in_dialog(dialog, &name);
    fj_log(&format!(
        "locate: nav={nav} uia={uia} set_filename={set} took={:?} path={path}",
        t0.elapsed()
    ));
    nav && (uia || set)
}

// ================= 前台轮询（自动弹出） =================

/// watcher 配置快照（设置保存时更新，轮询线程每轮读取）。
static WATCH_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static WATCH_AUTOPOPUP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static WATCH_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn watch_set(enabled: bool, auto_popup: bool, delay_ms: u64) {
    use std::sync::atomic::Ordering;
    WATCH_ENABLED.store(enabled, Ordering::SeqCst);
    WATCH_AUTOPOPUP.store(auto_popup, Ordering::SeqCst);
    WATCH_DELAY_MS.store(delay_ms, Ordering::SeqCst);
}

/// 对话框前台轮询：400ms 一次；同一 hwnd 只触发一次，切走后重置。
/// 主弹窗/QF 可见时（VISIBLE 共享位）跳过，避免抢键路由。
pub fn spawn_watcher(tx: Sender<AppEvt>) {
    std::thread::Builder::new()
        .name("clipx-fj-watch".into())
        .spawn(move || {
            use std::sync::atomic::Ordering;
            let mut last_fired: isize = 0;
            loop {
                std::thread::sleep(Duration::from_millis(400));
                if !WATCH_ENABLED.load(Ordering::SeqCst)
                    || !WATCH_AUTOPOPUP.load(Ordering::SeqCst)
                {
                    continue;
                }
                if crate::keyboard_hook::is_visible() {
                    continue;
                }
                #[cfg(windows)]
                let found = clipx_filejump::dialog::win::resolve_from_foreground();
                #[cfg(not(windows))]
                let found: Option<isize> = None;
                match found {
                    Some(d) if d != last_fired => {
                        last_fired = d;
                        // 延时内再确认一次前台仍是同一对话框（WPF ShowDelay 语义）。
                        let delay = WATCH_DELAY_MS.load(Ordering::SeqCst).max(80);
                        std::thread::sleep(Duration::from_millis(delay));
                        #[cfg(windows)]
                        let still = clipx_filejump::dialog::win::resolve_from_foreground() == Some(d);
                        #[cfg(not(windows))]
                        let still = false;
                        if still && !crate::keyboard_hook::is_visible() {
                            let _ = tx.send(AppEvt::FileJumpAuto(d));
                        }
                    }
                    None => {
                        last_fired = 0;
                    }
                    _ => {}
                }
            }
        })
        .ok();
}

// ================= WinEvent 实时跟随（对齐 WPF dock follow hooks） =================
//
// WPF 方案：全局 WinEvent 钩子（OUTOFCONTEXT|SKIPOWNPROCESS，pid/tid=0）
// 监听 MOVESIZE 范围 + LOCATIONCHANGE，按 DockEventBelongsToOwner 过滤
// （事件窗与 owner 同 root 且是 root 本体或 owner 本体），命中即重算 dock；
// 另有 OWNER DESTROY 钩子关窗 + 500ms timer 兜底。
// Rust 侧：独立线程泵消息（OUTOFCONTEXT 回调落在本线程），kill event 退出。
// 我们钩 LOCATIONCHANGE + DESTROY（MOVE/SIZE 由 LOCATIONCHANGE 覆盖；
// 焦点窃取抑制不需要——我们从不抢焦点），200ms tick 保留作兜底。
#[cfg(windows)]
mod follow {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::Mutex;

    use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
    use windows::Win32::UI::Accessibility::{
        SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetAncestor, IsWindow, MsgWaitForMultipleObjects, PeekMessageW,
        TranslateMessage, GA_ROOT, MSG, PM_REMOVE, QS_ALLINPUT, EVENT_OBJECT_DESTROY,
        EVENT_OBJECT_LOCATIONCHANGE,
    };

    use crate::logic::AppEvt;

    // WINEVENT_OUTOFCONTEXT=0x0000 | WINEVENT_SKIPOWNPROCESS=0x0002
    //（windows 0.59 未导出具名常量，此处按 SDK 原值定义）。
    const FLAGS: u32 = 0x0000 | 0x0002;

    static TX: Mutex<Option<std::sync::mpsc::Sender<AppEvt>>> = Mutex::new(None);
    static DLG: AtomicIsize = AtomicIsize::new(0);

    fn belongs(hwnd: HWND, dlg: HWND) -> bool {
        unsafe {
            if !IsWindow(Some(dlg)).as_bool() {
                return false;
            }
            let owner_root = GetAncestor(dlg, GA_ROOT);
            if owner_root.0.is_null() {
                return false;
            }
            let event_root = GetAncestor(hwnd, GA_ROOT);
            if event_root.0.is_null() || event_root != owner_root {
                return false;
            }
            hwnd == owner_root || hwnd == dlg
        }
    }

    unsafe extern "system" fn callback(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        _id_obj: i32,
        _id_child: i32,
        _tid: u32,
        _time: u32,
    ) {
        let dlg_isize = DLG.load(Ordering::SeqCst);
        if dlg_isize == 0 || hwnd.0.is_null() {
            return;
        }
        let dlg = HWND(dlg_isize as *mut _);
        if event == EVENT_OBJECT_DESTROY {
            if hwnd == dlg {
                if let Some(tx) = TX.lock().unwrap().as_ref() {
                    let _ = tx.send(AppEvt::FjDialogGone);
                }
            }
            return;
        }
        if event == EVENT_OBJECT_LOCATIONCHANGE && belongs(hwnd, dlg) {
            if let Some(tx) = TX.lock().unwrap().as_ref() {
                let _ = tx.send(AppEvt::FjDialogMoved);
            }
        }
    }

    /// 启动跟随线程，返回 kill event 句柄（存 isize）；hide 时 `stop`。
    pub fn spawn(tx: std::sync::mpsc::Sender<AppEvt>, dialog: isize) -> isize {
        let kill = unsafe {
            match CreateEventW(None, true, false, None) {
                Ok(h) => h,
                Err(_) => return 0,
            }
        };
        // HANDLE（*mut c_void）不是 Send：传原始值，线程内重建。
        let kill_val = kill.0 as isize;
        *TX.lock().unwrap() = Some(tx);
        DLG.store(dialog, Ordering::SeqCst);
        std::thread::Builder::new()
            .name("clipx-fj-follow".into())
            .spawn(move || unsafe {
                let kill = HANDLE(kill_val as *mut _);
                let h1 = SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(callback),
                    0,
                    0,
                    FLAGS,
                );
                let h2 = SetWinEventHook(
                    EVENT_OBJECT_DESTROY,
                    EVENT_OBJECT_DESTROY,
                    None,
                    Some(callback),
                    0,
                    0,
                    FLAGS,
                );
                loop {
                    let ret =
                        MsgWaitForMultipleObjects(Some(&[kill]), false, INFINITE, QS_ALLINPUT);
                    if ret == WAIT_OBJECT_0 {
                        break;
                    }
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                if !h1.0.is_null() {
                    let _ = UnhookWinEvent(h1);
                }
                if !h2.0.is_null() {
                    let _ = UnhookWinEvent(h2);
                }
                let _ = windows::Win32::Foundation::CloseHandle(kill);
            })
            .ok();
        kill_val
    }

    pub fn stop(kill_isize: isize) {
        if kill_isize == 0 {
            return;
        }
        unsafe {
            let _ = SetEvent(HANDLE(kill_isize as *mut _));
        }
    }
}

/// 启动 WinEvent 跟随线程（dialog=0 时不启动）；返回 kill 句柄。
pub fn follow_spawn(tx: Sender<AppEvt>, dialog: isize) -> isize {
    #[cfg(windows)]
    {
        if dialog == 0 {
            return 0;
        }
        follow::spawn(tx, dialog)
    }
    #[cfg(not(windows))]
    {
        let _ = (tx, dialog);
        0
    }
}

/// 停止跟随线程（hide 时调用；线程负责 unhook + 关闭句柄）。
pub fn follow_stop(kill: isize) {
    #[cfg(windows)]
    {
        follow::stop(kill);
    }
    #[cfg(not(windows))]
    {
        let _ = kill;
    }
}

// ================= 设置持久化（收藏/最近） =================

pub fn persist_lists(
    path: &Path,
    favs: &[crate::settings::FolderFavorite],
    recent: &[String],
    recent_max: usize,
) {
    let mut s: Settings = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    s.filejump_favorites = favs.to_vec();
    let mut r = recent.to_vec();
    r.truncate(recent_max.max(1));
    s.filejump_recent = r;
    let _ = settings::save(path, &s);
}

pub fn push_recent(path: &Path, recent: &mut Vec<String>, dir: &str, recent_max: usize) {
    recent.retain(|p| p.to_lowercase() != dir.to_lowercase());
    recent.insert(0, dir.to_string());
    recent.truncate(recent_max.max(1));
    let favs: Vec<crate::settings::FolderFavorite> = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .map(|s| s.filejump_favorites)
        .unwrap_or_default();
    persist_lists(path, &favs, recent, recent_max);
}

// ================= UI 推送 =================

#[allow(clippy::too_many_arguments)]
pub fn push_fj_ui(
    weak: &slint::Weak<crate::FileJumpWindow>,
    st: &FjState,
    show: bool,
    anchor_dialog: isize,
) {
    // FjRow is plain data (Send); ModelRc is built on the event-loop thread.
    // 主行永远是路径（管理器标签/收藏别名只进次行），对齐 WPF PathLine 语义。
    // split_hit 只算可见窗口（first_visible..first_visible+PAGE+1），其余行直接给全文
    // ——滚动时 push_fj_ui 会重算，不丢高亮。
    let vis_lo = st.first_visible;
    let vis_hi = st.first_visible + PAGE + 1;
    let ql = st.query.to_lowercase();
    let rows: Vec<crate::FjRow> = st
        .filtered
        .iter()
        .enumerate()
        .map(|(i, &idx)| {
            let c = &st.items[idx];
            let label = source_label(c.source);
            // 主行放**名称**（文件/文件夹最后一段），次行放父目录：
            // 全路径放主行时，命中段一长就把后半截顶出可视区（2026-10-23 实测）。
            let (name, parent) = split_display_name(&c.path);
            let sub: String = match c.alias.as_deref() {
                Some(a) if !a.is_empty() => format!("{parent}  ·  {a}"),
                _ => format!("{parent}  ·  {label}"),
            };
            let visible = i >= vis_lo && i < vis_hi;
            let (hit_pre, hit, hit_post) = if visible {
                let (pre, h, post) = crate::logic::split_hit(&name, &ql);
                // 命中段必须留在可视宽度内：前后各留少量上下文。
                (
                    tail_ellipsis(&pre, 18),
                    h,
                    head_ellipsis(&post, 18),
                )
            } else {
                (String::new(), String::new(), name.clone())
            };
            let (sub_pre, sub_hit, sub_post) = if visible {
                let (pre, h, post) = crate::logic::split_hit(&sub, &ql);
                if h.is_empty() {
                    (String::new(), String::new(), String::new())
                } else {
                    (tail_ellipsis(&pre, 20), h, head_ellipsis(&post, 20))
                }
            } else {
                (String::new(), String::new(), String::new())
            };
            crate::FjRow {
                alias: name.into(),
                path: sub.into(),
                source: label.into(),
                index_label: {
                    let rel = i as i32 - st.first_visible as i32 + 1;
                    if (1..=9).contains(&rel) {
                        rel.to_string().into()
                    } else {
                        "".into()
                    }
                },
                source_kind: source_kind(c.source),
                hit_pre: hit_pre.into(),
                hit: hit.into(),
                hit_post: hit_post.into(),
                sub_pre: sub_pre.into(),
                sub_hit: sub_hit.into(),
                sub_post: sub_post.into(),
            }
        })
        .collect();
    let n = st.filtered.len();
    let title: slint::SharedString = if st.global_mode {
        "跳转到文件夹（全局）".into()
    } else if matches!(st.intent, DialogIntent::Files) {
        "跳转 / 定位文件".into()
    } else {
        "跳转到文件夹".into()
    };
    let shown = n.min(PAGE).max(1) as f32;
    let h = (FJ_UI_CHROME + shown * FJ_ROW_H).clamp(FJ_MIN_H, FJ_MAX_H);
    let selected = st.selected as i32;
    let first_visible = st.first_visible as i32;
    let search: slint::SharedString = st.query.clone().into();
    let count: slint::SharedString = format!("{} 个", n).into();
    let hint: slint::SharedString = if st.navigating {
        "正在跳转…".into()
    } else if st.hint.is_empty() {
        "↑↓ 选择 · ←→ 翻页 · Ctrl+N 跳转 · Enter 跳转 · Del 移除收藏/最近 · Esc 关闭".into()
    } else {
        st.hint.clone().into()
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        use slint::{ModelRc, VecModel};
        let Some(ui) = weak.upgrade() else { return };
        if show {
            crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        }
        ui.set_title_label(title);
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_selected_index(selected);
        ui.set_search_text(search);
        ui.set_count_label(count);
        ui.set_hint_label(hint);
        ui.set_first_visible(first_visible);
        let win_w = FJ_W + FJ_CHROME * 2.0;
        let win_h = h + FJ_CHROME * 2.0;
        if show {
            // 顺序要紧：先在**隐藏态**把高度抖小 1px，再 show（首帧即重建缓冲、全量重绘），
            // 下一帧再恢复目标高度。详见 win_popup::force_full_repaint_before_show 注释。
            crate::win_popup::force_full_repaint_before_show(&ui.window(), win_w, win_h);
        } else {
            ui.window().set_size(slint::WindowSize::Logical(slint::LogicalSize::new(win_w, win_h)));
        }
        if show {
            #[cfg(windows)]
            if FOLLOW_MOUSE.load(Ordering::SeqCst) || anchor_dialog == 0 {
                crate::win_popup::position_near_cursor(&ui.window(), win_w, win_h);
            } else {
                crate::win_popup::position_dock_dialog(&ui.window(), anchor_dialog, win_w, win_h);
            }
            #[cfg(not(windows))]
            crate::win_popup::position_near_cursor(&ui.window(), win_w, win_h);
            let _ = ui.window().show();
            crate::win_popup::restore_size_after_show(&weak, win_w, win_h);
            #[cfg(windows)]
            {
                // Slint 的 raw_window_handle 在本工程下报 "cannot be represented"
                // 拿不到 HWND，FJ_HWND 曾一直为 0（dock 跟随/重停靠全失效），
                // 改枚举按标题取。
                if let Some(hwnd) = crate::win_popup::find_window_by_title("clipx-filejump") {
                    crate::mouse_hook::FJ_HWND
                        .store(hwnd, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    });
}

pub fn hide_fj_ui(weak: &slint::Weak<crate::FileJumpWindow>) {
    #[cfg(windows)]
    crate::mouse_hook::FJ_HWND.store(0, std::sync::atomic::Ordering::SeqCst);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().hide();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        // 每个测试独立目录：并行跑时共用同一路径会互相删除（曾经 flaky）。
        let dir = std::env::temp_dir().join(format!("clipx-fj-child-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dept-data")).unwrap();
        std::fs::create_dir_all(dir.join("readme")).unwrap();
        std::fs::write(dir.join("01_题库试题数据.xlsx"), b"x").unwrap();
        std::fs::write(dir.join("dept-notes.txt"), b"x").unwrap();
        dir
    }

    /// 无索引服务时的退路：能下钻到第 3 层，但不越过预算继续深挖。
    #[test]
    fn deep_local_walks_subdirs_up_to_depth_three() {
        let dir = scratch_dir("deep");
        let lvl3 = dir.join("a").join("b").join("c");
        std::fs::create_dir_all(&lvl3).unwrap();
        std::fs::write(lvl3.join("deep-target.txt"), b"x").unwrap();
        let lvl4 = lvl3.join("d");
        std::fs::create_dir_all(&lvl4).unwrap();
        std::fs::write(lvl4.join("too-deep-target.txt"), b"x").unwrap();

        let hits = deep_local_items(dir.to_str().unwrap(), "deep-target", 20, true);
        assert!(
            hits.iter().any(|c| c.path.ends_with("deep-target.txt")),
            "第 3 层应命中，got {:?}",
            hits.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
        assert!(
            !hits.iter().any(|c| c.path.contains("too-deep-target")),
            "第 4 层不该被遍历"
        );
        assert!(hits.iter().all(|c| c.source == CandidateSource::Local));
    }

    /// 选文件夹框（include_files=false）递归时只回目录，不回文件。
    #[test]
    fn deep_local_honours_folder_only_intent() {
        let dir = scratch_dir("deepfolders");
        let sub = dir.join("nested").join("目标目录");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("目标文件.txt"), b"x").unwrap();

        let hits = deep_local_items(dir.to_str().unwrap(), "目标", 20, false);
        assert!(hits.iter().all(|c| !c.is_file), "不应出现文件");
        assert!(
            hits.iter().any(|c| c.path.ends_with("目标目录")),
            "应命中子目录"
        );
    }

    #[test]
    fn display_name_splits_last_segment() {        assert_eq!(
            split_display_name(r"C:\Users\chaoj\Documents\wechat_files\machuntian\001_6626.mm"),
            (
                "001_6626.mm".to_string(),
                r"C:\Users\chaoj\Documents\wechat_files\machuntian".to_string()
            )
        );
        assert_eq!(
            split_display_name(r"C:\Users\chaoj\Desktop\新建文件夹\"),
            ("新建文件夹".to_string(), r"C:\Users\chaoj\Desktop".to_string())
        );
        // 盘符根不拆。
        assert_eq!(split_display_name(r"C:\"), (r"C:\".to_string(), String::new()));
    }

    #[test]
    fn ellipsis_keeps_hit_context() {
        assert_eq!(tail_ellipsis("abcdefghij", 3), "…hij");
        assert_eq!(head_ellipsis("abcdefghij", 3), "abc…");
        assert_eq!(tail_ellipsis("ab", 3), "ab");
    }

    #[test]
    fn folders_intent_skips_files_and_matches_name() {
        let dir = scratch_dir("folders");
        let hits = list_child_items(&dir.to_string_lossy(), "dept", 10, false);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.to_lowercase().replace('/', "\\").ends_with("dept-data"));
        assert_eq!(hits[0].source, CandidateSource::Current);
        assert!(!hits[0].is_file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_intent_includes_files() {
        let dir = scratch_dir("files");
        let hits = list_child_items(&dir.to_string_lossy(), "dept", 10, true);
        // dept-data（目录）+ dept-notes.txt（文件）。
        assert_eq!(hits.len(), 2);
        assert!(hits[0].is_folder()); // 目录排前面
        assert!(hits[1].is_file);
        let xlsx = list_child_items(&dir.to_string_lossy(), "01", 10, true);
        assert_eq!(xlsx.len(), 1);
        assert!(xlsx[0].is_file);
        assert!(xlsx[0].path.ends_with("01_题库试题数据.xlsx"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
