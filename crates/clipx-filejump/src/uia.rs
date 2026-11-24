//! WPS 自定义框 UIA 直驱（listary §5.2 + 映射表 #7）。
//!
//! WPS（Qt 自绘，不走 #32770）永不注入：此前只有键盘回退（Alt+D→键入→Enter）。
//! 本模块加 UIA 直驱：按 Qt 类名链定位路径组合框
//! （`KcfdFileDialog` → `KcfdFilterWidget` → `KcfdComboBox` → `QLineEdit`，
//! 回退 `kd::KDTextField`），ValuePattern 读写 + 读回校验。
//! 写通道只负责"把路径写进框"；回车与跳转校验走既有键盘链。
//! COM 按调用初始化/反初始化（低频对话框操作，无常驻成本）。

/// 候选编辑框打分（纯函数，单测锁定）：无 ValuePattern 直接 0（读写都指望它，
/// 没 pattern 的行调用方直接跳过）；Qt 路径链类名优先，非空值再加权。
pub fn wps_edit_score(class_name: &str, has_value_pattern: bool, value_non_empty: bool) -> i32 {
    if !has_value_pattern {
        return 0;
    }
    let mut s = 2;
    if class_name.contains("QLineEdit") || class_name.contains("KcfdComboBox") {
        s += 3;
    } else if class_name.contains("KDTextField") {
        s += 2;
    } else if class_name.contains("Edit") {
        s += 1;
    }
    if value_non_empty {
        s += 1;
    }
    s
}

#[cfg(windows)]
mod imp {
    use anyhow::{bail, Result};
    use windows::core::BSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Descendants,
        UIA_ValuePatternId,
    };

    /// COM 作用域守卫：进出配对（低频调用，按次初始化）。
    struct ComGuard {
        ok: bool,
    }

    impl ComGuard {
        fn enter() -> Self {
            let ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            Self { ok }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.ok {
                unsafe { CoUninitialize() };
            }
        }
    }

    fn automation() -> Result<IUIAutomation> {
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| anyhow::anyhow!("CUIAutomation 失败: {e:?}"))
    }

    fn class_name_of(el: &IUIAutomationElement) -> String {
        unsafe { el.CurrentClassName() }
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    fn value_of(el: &IUIAutomationElement) -> Option<String> {
        let pat = unsafe {
            el.GetCurrentPatternAs::<
                windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
            >(UIA_ValuePatternId)
        }
        .ok()?;
        unsafe { pat.CurrentValue() }.ok().map(|v| v.to_string())
    }

    fn has_value_pattern(el: &IUIAutomationElement) -> bool {
        unsafe {
            el.GetCurrentPatternAs::<
                windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
            >(UIA_ValuePatternId)
        }
        .is_ok()
    }

    /// 对话框下找路径组合框：Qt 类名链优先（FilterWidget→ComboBox→QLineEdit），
    /// 找不到走通用回退（可见 Edit 打分最高者）。上限 400 个节点。
    fn find_path_combo(
        auto: &IUIAutomation,
        dialog: HWND,
    ) -> Result<IUIAutomationElement> {
        let root = unsafe { auto.ElementFromHandle(dialog) }
            .map_err(|e| anyhow::anyhow!("ElementFromHandle 失败: {e:?}"))?;
        let cond = unsafe { auto.CreateTrueCondition() }
            .map_err(|e| anyhow::anyhow!("TrueCondition 失败: {e:?}"))?;
        let all = unsafe { root.FindAll(TreeScope_Descendants, &cond) }
            .map_err(|e| anyhow::anyhow!("FindAll 失败: {e:?}"))?;
        let n = unsafe { all.Length() }.unwrap_or(0).min(400);
        // 第一遍：Qt 类名链（FilterWidget 内找 ComboBox/QLineEdit）。
        let mut filter_widgets = Vec::new();
        let mut edits = Vec::new();
        for i in 0..n {
            let Ok(el) = (unsafe { all.GetElement(i) }) else {
                continue;
            };
            let cls = class_name_of(&el);
            if cls.contains("KcfdFilterWidget") {
                filter_widgets.push(el);
            } else if cls.contains("Edit") || cls.contains("ComboBox") {
                edits.push((cls, el));
            }
        }
        // FilterWidget 内优先。
        for fw in &filter_widgets {
            let Ok(inner) = (unsafe { fw.FindAll(TreeScope_Descendants, &cond) })
            else {
                continue;
            };
            let m = unsafe { inner.Length() }.unwrap_or(0).min(100);
            for i in 0..m {
                let Ok(el) = (unsafe { inner.GetElement(i) }) else {
                    continue;
                };
                let cls = class_name_of(&el);
                if cls.contains("QLineEdit") || cls.contains("KcfdComboBox") {
                    return Ok(el);
                }
            }
        }
        // 通用回退：打分最高且有 ValuePattern 的。
        let mut best: Option<(i32, IUIAutomationElement)> = None;
        for (cls, el) in &edits {
            let has_vp = has_value_pattern(el);
            if !has_vp {
                continue;
            }
            let v = value_of(el).unwrap_or_default();
            let s = super::wps_edit_score(cls, true, !v.is_empty());
            if best.as_ref().is_none_or(|(bs, _)| s > *bs) {
                best = Some((s, el.clone()));
            }
        }
        best
            .map(|(_, el)| el)
            .ok_or_else(|| anyhow::anyhow!("找不到路径组合框"))
    }

    /// 读 WPS 对话框路径组合框当前文本（导航成功校验用）。
    pub fn path_combo_text(dialog_isize: isize) -> Result<String> {
        let _com = ComGuard::enter();
        let auto = automation()?;
        let el = find_path_combo(&auto, HWND(dialog_isize as *mut _))?;
        value_of(&el).ok_or_else(|| anyhow::anyhow!("无 ValuePattern 文本"))
    }

    /// 写 WPS 对话框路径组合框 + 150ms 后读回校验（Qt 异步 frame，需等一拍）。
    /// 读回不一致即 false（调用方回退键盘链）；不可读按失败处理
    /// （文件名框与密码框不同，QLineEdit 读回必然可读——读不到就是没写上）。
    pub fn path_combo_set(dialog_isize: isize, folder: &str) -> Result<bool> {
        if folder.is_empty() {
            bail!("空路径不写");
        }
        let _com = ComGuard::enter();
        let auto = automation()?;
        let el = find_path_combo(&auto, HWND(dialog_isize as *mut _))?;
        let pat = unsafe {
            el.GetCurrentPatternAs::<
                windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
            >(UIA_ValuePatternId)
        }
        .map_err(|e| anyhow::anyhow!("无 ValuePattern: {e:?}"))?;
        unsafe { pat.SetValue(&BSTR::from(folder)) }
            .map_err(|e| anyhow::anyhow!("SetValue 失败: {e:?}"))?;
        std::thread::sleep(std::time::Duration::from_millis(150));
        match unsafe { pat.CurrentValue() } {
            Ok(v) => Ok(v.to_string() == folder),
            Err(e) => bail!("读回失败: {e:?}"),
        }
    }
}

#[cfg(windows)]
pub use imp::{path_combo_set, path_combo_text};

#[cfg(test)]
mod tests {
    use super::wps_edit_score;

    #[test]
    fn qt_chain_scores_highest() {
        assert!(wps_edit_score("QLineEdit", true, true) > wps_edit_score("Edit", true, true));
        assert!(wps_edit_score("KcfdComboBox", true, false) > wps_edit_score("Edit", true, true));
        assert!(wps_edit_score("KDTextField", true, true) > wps_edit_score("Edit", true, true));
        // 非空值加权（同类名下有内容的优先）。
        assert!(wps_edit_score("Edit", true, true) > wps_edit_score("Edit", true, false));
        // 无 ValuePattern 的直接归零（调用方跳过；类名再对也白搭）。
        assert_eq!(wps_edit_score("QLineEdit", false, false), 0);
        assert_eq!(wps_edit_score("QLineEdit", false, true), 0);
    }
}
