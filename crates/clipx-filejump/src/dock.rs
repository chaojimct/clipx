//! Picker 停靠位置（M5d）：逐行对齐 WPF `FileJumpPickerDockPlacement`。
//!
//! 规则（物理像素，gap=4）：
//! 1. 右侧：`x = dr.Right + 4, y = dr.Top`，放得下即用
//! 2. 左侧：`x = dr.Left - popupW - 4`（y 不变）
//! 3. 左右都放不下 → 底部居中：`x = 中心 - popupW/2, y = dr.Bottom + 4`
//! 4. 夹紧到工作区（x 左右夹，y 上下夹）
//!
//! 纯函数，跨平台可测；显示器工作区由调用方传入。

/// 对话框矩形（左/上/右/下，物理像素）。
pub type Rect = (i32, i32, i32, i32);

pub fn dock_position(dlg: Rect, popup_w: i32, popup_h: i32, work: Rect) -> (i32, i32) {
    const GAP: i32 = 4;
    let (l, t, r, b) = dlg;
    let (wl, wt, wr, wb) = work;

    // 先尝试右侧（y 与对话框顶部对齐）。
    let mut x = r + GAP;
    let mut y = t;
    let fits_right = x + popup_w <= wr;

    // 右侧放不下，尝试左侧。
    if !fits_right {
        x = l - popup_w - GAP;
    }

    // 左右都放不下，底部居中。
    if x < wl || x + popup_w > wr {
        x = (l + r) / 2 - popup_w / 2;
        y = b + GAP;
    }

    // 夹紧。
    if x < wl {
        x = wl;
    }
    if x + popup_w > wr {
        x = (wr - popup_w).max(wl);
    }
    if y < wt {
        y = wt;
    }
    if y + popup_h > wb {
        y = wb - popup_h;
    }
    let _ = popup_h;
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: Rect = (0, 0, 1920, 1040);

    #[test]
    fn prefers_right_top_aligned() {
        // 对话框 100..600，picker 宽 500：右侧 604+500 <= 1920。
        assert_eq!(dock_position((100, 100, 600, 500), 500, 300, WORK), (604, 100));
    }

    #[test]
    fn falls_back_to_left() {
        // 对话框贴右：右侧放不下 → 左侧。
        assert_eq!(
            dock_position((1300, 100, 1900, 500), 500, 300, WORK),
            (1300 - 500 - 4, 100)
        );
    }

    #[test]
    fn bottom_center_when_neither_side_fits() {
        // 超宽对话框：左右都放不下 → 底部居中。
        assert_eq!(
            dock_position((0, 100, 1920, 500), 500, 300, WORK),
            (960 - 250, 504)
        );
    }

    #[test]
    fn clamps_into_work_area() {
        // 底部放不下 → y 上夹（可能盖住对话框，与 WPF 一致）。
        let (x, y) = dock_position((0, 100, 1920, 900), 500, 300, WORK);
        assert_eq!((x, y), (710, 1040 - 300));
    }
}
