//! TSF「光标信标」共享内存协议（v2）
//!
//! 信标 DLL（`crates/clipx-tsf`）被 TSF 注入到宿主进程（WorkBuddy / 微信 / Cursor …），
//! 在宿主进程内用 `ITfContextView::GetTextExt` 读出**真实插入点矩形**，
//! 写进共享内存 `Local\clipx-tsf-caret`；clipx 侧读它即可拿到像素级精确的 caret 位置，
//! 绕开 UIA/IMM 在 Electron 应用上「拿不到自绘 caret」的死结。
//!
//! 布局：64 槽 × 64 字节。
//!
//! 槽 0 = 控制块
//! ```text
//! [0] magic 0x43424332 ("BC2")
//! [1] version
//! [2] request_seq   clipx 递增，表示「请测一次」
//! [3] request_tick_lo / [4] request_tick_hi
//! [5] ack_seq       信标已应答的 request_seq
//! [6] flags
//! ```
//!
//! 槽 1..=63 = 测量结果环形缓冲
//! ```text
//! [0] magic 0x43424331 ("BC1")
//! [1] pid           宿主进程 id
//! [2] tick_lo / [3] tick_hi
//! [4..7] rect l,t,r,b（宿主进程客户区坐标系）
//! [8] dpi_aware
//! [9] seq
//! [10] source（0=激活期测量，1=常驻应答）
//! ```
//!
//! 本模块是纯 Rust（不依赖 windows crate），三平台 CI 都能编译并跑单测。

pub const SHM_NAME: &str = r"Local\clipx-tsf-caret";

pub const SLOT_COUNT: usize = 64;
pub const SLOT_SIZE: usize = 64;

pub const MAGIC_CTRL: u32 = 0x4342_4332; // "BC2"
pub const MAGIC_SLOT: u32 = 0x4342_4331; // "BC1"
pub const PROTOCOL_VERSION: u32 = 1;

/// 控制块槽的各字段在 u32 数组中的下标。
pub mod ctrl {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 1;
    pub const REQUEST_SEQ: usize = 2;
    pub const REQUEST_TICK_LO: usize = 3;
    pub const REQUEST_TICK_HI: usize = 4;
    pub const ACK_SEQ: usize = 5;
    pub const FLAGS: usize = 6;
}

/// 结果槽的各字段在 u32 数组中的下标。
pub mod slot {
    pub const MAGIC: usize = 0;
    pub const PID: usize = 1;
    pub const TICK_LO: usize = 2;
    pub const TICK_HI: usize = 3;
    pub const RECT_L: usize = 4;
    pub const RECT_T: usize = 5;
    pub const RECT_R: usize = 6;
    pub const RECT_B: usize = 7;
    pub const DPI_AWARE: usize = 8;
    pub const SEQ: usize = 9;
    pub const SOURCE: usize = 10;
}

/// 测量来源。
pub const SOURCE_ACTIVATE: u32 = 0;
pub const SOURCE_REQUEST: u32 = 1;

/// 结果槽里的一个测量结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaretRect {
    /// 宿主进程 id
    pub pid: u32,
    /// 采集时刻（GetTickCount64 毫秒），跨进程同源，可用于新鲜度校验
    pub tick: u64,
    /// 插入点矩形（宿主客户区坐标系）
    pub rect: (i32, i32, i32, i32),
    /// 信标自述是否 DPI 感知
    pub dpi_aware: bool,
    /// 结果序号（槽内自增）
    pub seq: u32,
    /// 来源：0=激活期测量，1=常驻应答
    pub source: u32,
}

impl CaretRect {
    /// 矩形宽高是否合理（排除零高度 / 整块输入框这种噪声）。
    ///
    /// 注：**光标是竖线**，折叠选区的 `GetTextExt` 常给出 `left == right`（宽 0）
    /// 或 1~2px 宽，必须放行；只有高度才是有效性的硬指标。
    pub fn sane(&self) -> bool {
        let (l, t, r, b) = self.rect;
        let w = r - l;
        let h = b - t;
        w >= 0 && w < 4000 && h > 0 && h < 4000
    }

    /// 矩形中心点（宿主客户区坐标系）。
    pub fn center(&self) -> (i32, i32) {
        let (l, t, r, b) = self.rect;
        ((l + r) / 2, (t + b) / 2)
    }
}

/// 解析一个结果槽的原始字节（`SLOT_SIZE` 字节）。
///
/// 字节序：Windows 小端 u32。
pub fn parse_slot(buf: &[u8]) -> Option<CaretRect> {
    if buf.len() < SLOT_SIZE {
        return None;
    }
    let w: Vec<u32> = buf
        .chunks_exact(4)
        .take(SLOT_SIZE / 4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if w[slot::MAGIC] != MAGIC_SLOT {
        return None;
    }
    let rect = (
        w[slot::RECT_L] as i32,
        w[slot::RECT_T] as i32,
        w[slot::RECT_R] as i32,
        w[slot::RECT_B] as i32,
    );
    Some(CaretRect {
        pid: w[slot::PID],
        tick: (w[slot::TICK_LO] as u64) | ((w[slot::TICK_HI] as u64) << 32),
        rect,
        dpi_aware: w[slot::DPI_AWARE] != 0,
        seq: w[slot::SEQ],
        source: w[slot::SOURCE],
    })
}

/// 从一批结果槽中挑出「目标 pid 且最新」的一个。
///
/// `slots` 顺序任意；同 pid 多条时取 tick 最大的；tick 相同则取 seq 最大的。
pub fn pick_latest(slots: &[Option<CaretRect>], want_pid: u32) -> Option<CaretRect> {
    slots
        .iter()
        .flatten()
        .filter(|c| c.pid == want_pid)
        .filter(|c| c.sane())
        .max_by(|a, b| a.tick.cmp(&b.tick).then(a.seq.cmp(&b.seq)))
        .copied()
}

/// 结果是否足够新鲜（毫秒）。超过则视为陈旧，丢弃。
pub fn is_fresh(c: &CaretRect, now_tick: u64, max_age_ms: u64) -> bool {
    now_tick >= c.tick && now_tick - c.tick <= max_age_ms
}

// ---------------------------------------------------------------------------
// 协议 v1 兼容
//
// 早期 spike 版 DLL（caret-beacon-spike）用的是 v1 布局：
// **槽 0 起即为结果槽**，没有控制块、没有请求/应答。
// 机器上可能仍注册着旧 DLL，因此客户端要先探测是 v1 还是 v2。
// ---------------------------------------------------------------------------

/// v1 协议版本号（写在槽 0 的 version 位）。
pub const PROTOCOL_VERSION_V1: u32 = 1;

/// 探测共享内存当前用的是 v1 还是 v2。
///
/// 返回 `Some(2)` = v2（有控制块），`Some(1)` = v1（槽 0 即结果），`None` = 无有效内容。
pub fn detect_version(words: &[u32]) -> Option<u32> {
    let magic = *words.first()?;
    if magic == MAGIC_CTRL {
        return Some(2);
    }
    if magic == MAGIC_SLOT {
        return Some(1);
    }
    None
}

/// 解析 v1 布局的一个结果槽（字段偏移与 v2 的结果槽相同）。
pub fn parse_slot_v1(words: &[u32]) -> Option<CaretRect> {
    if words.len() < SLOT_SIZE / 4 {
        return None;
    }
    if words[slot::MAGIC] != MAGIC_SLOT {
        return None;
    }
    let rect = (
        words[slot::RECT_L] as i32,
        words[slot::RECT_T] as i32,
        words[slot::RECT_R] as i32,
        words[slot::RECT_B] as i32,
    );
    Some(CaretRect {
        pid: words[slot::PID],
        tick: (words[slot::TICK_LO] as u64) | ((words[slot::TICK_HI] as u64) << 32),
        rect,
        dpi_aware: words[slot::DPI_AWARE] != 0,
        seq: words[slot::SEQ],
        source: words[slot::SOURCE],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_slot(pid: u32, tick: u64, rect: (i32, i32, i32, i32), seq: u32) -> Vec<u8> {
        let mut w = [0u32; SLOT_SIZE / 4];
        w[slot::MAGIC] = MAGIC_SLOT;
        w[slot::PID] = pid;
        w[slot::TICK_LO] = tick as u32;
        w[slot::TICK_HI] = (tick >> 32) as u32;
        w[slot::RECT_L] = rect.0 as u32;
        w[slot::RECT_T] = rect.1 as u32;
        w[slot::RECT_R] = rect.2 as u32;
        w[slot::RECT_B] = rect.3 as u32;
        w[slot::DPI_AWARE] = 1;
        w[slot::SEQ] = seq;
        w[slot::SOURCE] = SOURCE_REQUEST;
        let mut buf = Vec::with_capacity(SLOT_SIZE);
        for x in w {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        buf
    }

    #[test]
    fn parses_valid_slot() {
        let buf = make_slot(4242, 123_456, (10, 20, 12, 40), 7);
        let c = parse_slot(&buf).expect("should parse");
        assert_eq!(c.pid, 4242);
        assert_eq!(c.tick, 123_456);
        assert_eq!(c.rect, (10, 20, 12, 40));
        assert_eq!(c.seq, 7);
        assert!(c.sane());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = make_slot(1, 1, (0, 0, 1, 1), 0);
        buf[0] = 0;
        assert!(parse_slot(&buf).is_none());
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(parse_slot(&[0u8; 16]).is_none());
    }

    #[test]
    fn picks_latest_for_pid() {
        let a = parse_slot(&make_slot(100, 500, (0, 0, 1, 10), 1)).unwrap();
        let b = parse_slot(&make_slot(100, 900, (0, 0, 2, 10), 2)).unwrap();
        let other = parse_slot(&make_slot(200, 950, (0, 0, 3, 10), 3)).unwrap();
        let got = pick_latest(&[Some(a), Some(b), Some(other)], 100).unwrap();
        assert_eq!(got.tick, 900);
    }

    #[test]
    fn rejects_degenerate_rect() {
        let c = parse_slot(&make_slot(1, 1, (5, 5, 5, 5), 0)).unwrap();
        assert!(!c.sane());
    }

    #[test]
    fn accepts_zero_width_caret() {
        // 折叠选区的真 caret 是竖线：left == right（宽 0）必须放行。
        let c = parse_slot(&make_slot(1, 1, (3547, 934, 3547, 953), 0)).unwrap();
        assert!(c.sane(), "零宽竖线 caret 应当被接受");
    }

    #[test]
    fn freshness_window() {
        let c = parse_slot(&make_slot(1, 1000, (0, 0, 1, 10), 0)).unwrap();
        assert!(is_fresh(&c, 1500, 600));
        assert!(!is_fresh(&c, 2000, 600));
    }

    #[test]
    fn detects_v1_and_v2() {
        let mut w = vec![0u32; SLOT_SIZE / 4];
        assert_eq!(detect_version(&w), None);
        w[0] = MAGIC_CTRL;
        assert_eq!(detect_version(&w), Some(2));
        w[0] = MAGIC_SLOT;
        assert_eq!(detect_version(&w), Some(1));
    }

    #[test]
    fn parses_v1_slot() {
        let buf = make_slot(77, 4242, (3, 4, 5, 24), 2);
        let words: Vec<u32> = buf
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let c = parse_slot_v1(&words).expect("v1 should parse");
        assert_eq!(c.pid, 77);
        assert_eq!(c.tick, 4242);
        assert_eq!(c.rect, (3, 4, 5, 24));
    }
}
