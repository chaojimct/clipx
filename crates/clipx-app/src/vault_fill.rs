//! 账号填充下拉窗（独立于剪贴板主弹窗）：贴着账号/密码输入框下沿展开，
//! 对标 Bitwarden 浏览器插件的候选下拉形态。
//!
//! - 常驻实例 Hide/Show 复用；无焦点三件套与 vaultkey/inline 同序
//!   （show → position → apply_style → position；apply_style 必须在 show 之后，
//!   winit 的 show 会重置 EXSTYLE）。
//! - 定位：输入框矩形下沿，底边溢出工作区时翻到上沿；只设位置不钉尺寸
//!   （尺寸留给 Slint/winit 按 scale 换算，钉物理尺寸在 200% 缩放下会把逻辑
//!   窗口砍半，见 CLAUDE.md UI 陷阱 #8）。
//! - 主题：各 Window 的 Theme global 独立，show 前按当前主题重刷。
//! - 快照：`--vault-fill-demo` 自检入口。

use slint::{ComponentHandle, ModelRc, VecModel};

/// 下拉行数据（Slint 侧只做展示；秘密永不进这里，只有名/用户名）。
pub struct FillRowData {
    pub letter: String,
    pub name: String,
    pub user: String,
}

/// 一帧快照（无状态，每次重算下发；选中序号由逻辑层 State 持有）。
/// `x/y` 物理像素；`w/h` 逻辑单位（Slint 布局与原生尺寸同源，
/// 由 show() 内 `set_win_w` + force/restore 落盘——两边口径不一致的那几十 px
/// 在透明 layered 窗上就是"右侧被切断"，见 2026-11-22 实测）。
pub struct FillSnapshot {
    pub rows: Vec<FillRowData>,
    pub selected: usize,
    /// "list" | "locked" | "empty"（Slint 侧按种渲染）。
    pub mode: &'static str,
    pub hint: String,
    pub state_text: String,
    /// 状态行图标字（"锁"/"搜"）与右侧动作按钮文案（"解锁"/"去搜索"）。
    pub state_icon: String,
    pub state_btn: String,
    pub x: i32,
    pub y: i32,
    pub w: f32,
    pub h: f32,
}

/// 首字母块：首个非空字符大写；空名兜底 "•"。
pub fn row_letter(name: &str) -> String {
    name.trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "•".to_string())
}

/// 下拉落点（纯函数，单测锁定）：输入框下沿 +4px 间隙，底边溢出翻上沿，
/// 左右钳进工作区。`rect=(l,t,w,h)` 物理像素；`work=(l,t,r,b)` 工作区。
pub fn fill_anchor(
    rect: (i32, i32, i32, i32),
    win_w: i32,
    win_h: i32,
    work: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (l, t, _w, h) = rect;
    let (wl, wt, wr, wb) = work;
    let x = l.clamp(wl, (wr - win_w).max(wl));
    let mut y = t + h + 4;
    if y + win_h > wb {
        y = t - win_h - 4;
    }
    if y < wt {
        y = wt;
    }
    (x, y)
}

/// 下拉宽度跟输入框（逻辑单位，调用方已按锚点屏 scale 换算；clamp 280~600）。
/// 对标 Bitwarden：与输入框等宽（窄屏才钳制）。
pub fn fill_width(field_w_logical: i32) -> i32 {
    field_w_logical.clamp(280, 600)
}

/// 显示/刷新（常驻实例；show 只在不可见时调，免重激活抢焦点）。
pub fn show(weak: &slint::Weak<crate::VaultFillWindow>, snap: FillSnapshot) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        crate::settings_win::paint_theme_handle(ui.global::<crate::Theme>());
        // 先定逻辑宽（Slint 布局与原生尺寸同源；否则透明区露馅被当"切断"）。
        ui.set_win_w(snap.w);
        #[cfg(windows)]
        crate::win_popup::force_full_repaint_before_show(ui.window(), snap.w, snap.h);
        let rows: Vec<crate::VaultFillRow> = snap
            .rows
            .iter()
            .map(|r| crate::VaultFillRow {
                letter: r.letter.as_str().into(),
                name: r.name.as_str().into(),
                user: r.user.as_str().into(),
            })
            .collect();
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_selected(snap.selected as i32);
        ui.set_mode(snap.mode.into());
        ui.set_hint(snap.hint.as_str().into());
        ui.set_state_text(snap.state_text.as_str().into());
        ui.set_state_icon(snap.state_icon.as_str().into());
        ui.set_state_btn(snap.state_btn.as_str().into());
        if !ui.window().is_visible() {
            let _ = ui.window().show();
        }
        ui.window().set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(snap.x, snap.y),
        ));
        #[cfg(windows)]
        {
            crate::win_popup::commit_hwnd_pos(ui.window(), snap.x, snap.y);
            crate::win_popup::apply_style(ui.window());
            crate::win_popup::commit_hwnd_pos(ui.window(), snap.x, snap.y);
        }
        #[cfg(windows)]
        crate::win_popup::restore_size_after_show(&weak, snap.w, snap.h);
    });
}

/// 隐藏（常驻实例复用，不销毁）。
pub fn hide(weak: &slint::Weak<crate::VaultFillWindow>) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().hide();
        }
    });
}

/// UI 自检用：把当前下拉窗渲染成 PNG（事件循环线程调用；没开返回 false）。
pub fn snapshot_current_to(path: &str) -> bool {
    CURRENT.with(|c| {
        let borrow = c.borrow();
        let Some(weak) = borrow.as_ref() else {
            return false;
        };
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        crate::save_snapshot(&ui.window(), path);
        true
    })
}

// 实例 weak 只存放在事件循环线程（Slint 组件的创建与 drop 都须在该线程）。
thread_local! {
    static CURRENT: std::cell::RefCell<Option<slint::Weak<crate::VaultFillWindow>>> =
        const { std::cell::RefCell::new(None) };
}

/// 创建时登记（main.rs 建窗后调一次；show/hide 走 LogicDeps 的 weak）。
pub fn remember(weak: slint::Weak<crate::VaultFillWindow>) {
    let _ = slint::invoke_from_event_loop(move || {
        CURRENT.with(|c| *c.borrow_mut() = Some(weak));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_letter_takes_first_upper() {
        assert_eq!(row_letter("github"), "G");
        assert_eq!(row_letter("  aliYun"), "A");
        assert_eq!(row_letter("微信"), "微");
        assert_eq!(row_letter(""), "•");
        assert_eq!(row_letter("   "), "•");
    }

    #[test]
    fn anchor_below_field_clamped_and_flips() {
        let work = (0, 0, 1920, 1040);
        // 常规：下沿 +4。
        assert_eq!(fill_anchor((100, 200, 300, 30), 300, 200, work), (100, 234));
        // 右溢出：左钳。
        assert_eq!(fill_anchor((1800, 200, 300, 30), 300, 200, work), (1620, 234));
        // 底溢出：翻上沿。
        assert_eq!(fill_anchor((100, 900, 300, 30), 300, 200, work), (100, 696));
        // 上下都放不下：钳工作区顶。
        assert_eq!(fill_anchor((100, 0, 300, 30), 300, 1200, work), (100, 0));
        // 左溢出：钳左。
        assert_eq!(fill_anchor((-50, 200, 300, 30), 300, 200, work), (0, 234));
    }

    #[test]
    fn width_follows_field() {
        assert_eq!(fill_width(200), 280);
        assert_eq!(fill_width(340), 340);
        assert_eq!(fill_width(800), 600);
    }
}
