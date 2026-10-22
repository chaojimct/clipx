//! TSF 光标信标的**激活器**（Windows）。
//!
//! 背景：信标 DLL 是 TSF TIP，TSF 只在「该 profile 被激活」时才把它注入宿主进程。
//! 但我们**不能**把用户的活动输入法切到信标上 —— 那会干扰正常输入。
//!
//! 解法：`ITfInputProcessorProfileMgr::ActivateProfile` 带三个 flag：
//! - `TF_IPPMF_FORSESSION`（0x20000000）—— 整个登录会话生效，
//!   **新启动的进程也会加载信标**（这正是 FORPROCESS 做不到的）。
//! - `TF_IPPMF_ENABLEPROFILE`（1）—— 启用该 profile。
//! - `TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE`（4）—— **不切换当前输入语言**，
//!   用户的微信输入法 / 搜狗等照常是活动输入法。
//!
//! 安全性论证（对应"会不会影响正常输入法"）：
//! 1. `DONTCARECURRENTINPUTLANGUAGE` 保证活动 profile 不变。
//! 2. 信标 `Activate` 期间把 keyboard openclose 置 0（IME 直通），
//!    `Deactivate` 立刻恢复为 1（铁律 2）。
//! 3. 信标的 `ITfKeyEventSink` 全部返回 FALSE —— 一个键都不吞。
//! 4. `Deactivate` 保留 sink，信标只是**只读观察者**。

use windows::core::{Interface, GUID};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::HKL;
use windows::Win32::UI::TextServices::*;

/// 信标 TIP 的 CLSID（与 `clipx-tsf` crate 一致）。
pub const CLSID_BEACON: GUID = GUID::from_u128(0xc4be0c0a_9326_4c1a_beac_0cbeac0dec01);
/// 信标的 language profile GUID。
pub const PROFILE_BEACON: GUID = GUID::from_u128(0xc4be0c0a_9326_4c1a_beac_0cbeac0dec02);
/// 键盘类 category（用于查询活动 profile）。
pub const CAT_KEYBOARD: GUID = GUID::from_u128(0x34745c63_b2f0_4784_8b67_5e12c8701a31);

const LANGID_ZH_CN: u16 = 0x0804;
const TF_PROFILETYPE_INPUTPROCESSOR: u32 = 1;

/// 取得 ITfInputProcessorProfileMgr。
///
/// `CLSID_TF_InputProcessorProfileMgr` 本身没有 COM 注册，
/// 得从 `CLSID_TF_InputProcessorProfiles` 单例 QI 出来。
fn profile_mgr() -> windows::core::Result<ITfInputProcessorProfileMgr> {
    unsafe {
        let profiles: ITfInputProcessorProfiles =
            CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
        profiles.cast::<ITfInputProcessorProfileMgr>()
    }
}

/// 激活信标 profile（会话级，不切换用户的当前输入语言）。
///
/// 返回 `Ok(())` 表示激活成功。重复调用是幂等的。
pub fn activate_beacon() -> windows::core::Result<()> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // RPC_E_CHANGED_MODE 说明线程已是其他公寓模型 —— 不致命，继续。
        let already = hr.is_ok();
        let result = (|| -> windows::core::Result<()> {
            let m = profile_mgr()?;
            m.ActivateProfile(
                TF_PROFILETYPE_INPUTPROCESSOR,
                LANGID_ZH_CN,
                &CLSID_BEACON,
                &PROFILE_BEACON,
                HKL(std::ptr::null_mut()),
                TF_IPPMF_FORSESSION
                    | TF_IPPMF_ENABLEPROFILE
                    | TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE,
            )
        })();
        if already {
            CoUninitialize();
        }
        result
    }
}

/// 查询当前活动的键盘类 profile（诊断用）。
pub fn active_profile() -> windows::core::Result<TF_INPUTPROCESSORPROFILE> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let already = hr.is_ok();
        let result = (|| -> windows::core::Result<TF_INPUTPROCESSORPROFILE> {
            let m = profile_mgr()?;
            let mut p = TF_INPUTPROCESSORPROFILE::default();
            m.GetActiveProfile(&CAT_KEYBOARD, &mut p)?;
            Ok(p)
        })();
        if already {
            CoUninitialize();
        }
        result
    }
}

/// 反激活信标 profile。
///
/// 用途：让 TSF 把信标 DLL 加载进宿主进程后，再把活动输入法还给用户
/// （信标只是只读观察者，Deactivate 后 sink 仍常驻）。
pub fn deactivate_beacon() -> windows::core::Result<()> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let already = hr.is_ok();
        let result = (|| -> windows::core::Result<()> {
            let m = profile_mgr()?;
            m.DeactivateProfile(
                TF_PROFILETYPE_INPUTPROCESSOR,
                LANGID_ZH_CN,
                &CLSID_BEACON,
                &PROFILE_BEACON,
                HKL(std::ptr::null_mut()),
                TF_IPPMF_FORSESSION,
            )
        })();
        if already {
            CoUninitialize();
        }
        result
    }
}

/// 把活动输入法切换到指定的 profile（用于把输入法还给用户）。
pub fn activate_profile(
    clsid: &GUID,
    guid_profile: &GUID,
    langid: u16,
) -> windows::core::Result<()> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let already = hr.is_ok();
        let result = (|| -> windows::core::Result<()> {
            let m = profile_mgr()?;
            m.ActivateProfile(
                TF_PROFILETYPE_INPUTPROCESSOR,
                langid,
                clsid,
                guid_profile,
                HKL(std::ptr::null_mut()),
                TF_IPPMF_FORSESSION | TF_IPPMF_ENABLEPROFILE,
            )
        })();
        if already {
            CoUninitialize();
        }
        result
    }
}

/// 微信输入法的 CLSID / profile（用于恢复用户的输入法）。
pub const CLSID_WETYPE: GUID = GUID::from_u128(0x86598fb9_66a2_463e_b9c2_aeb906d477ad);
pub const PROFILE_WETYPE: GUID = GUID::from_u128(0x607fdf85_fcc8_4dbd_a365_41296f980c9c);

/// 信标 profile 是否已注册并可用。
///
/// 实现方式：查 `ITfInputProcessorProfileMgr::GetProfile`（按 category + profile GUID 取），
/// 成功即说明系统认识这个 profile —— 通常意味着已注册且已启用。
pub fn beacon_available() -> bool {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let already = hr.is_ok();
        let result = (|| -> bool {
            let Ok(m) = profile_mgr() else { return false };
            let mut p = TF_INPUTPROCESSORPROFILE::default();
            m.GetProfile(
                TF_PROFILETYPE_INPUTPROCESSOR,
                LANGID_ZH_CN,
                &CLSID_BEACON,
                &PROFILE_BEACON,
                HKL(std::ptr::null_mut()),
                &mut p,
            )
            .is_ok()
        })();
        if already {
            CoUninitialize();
        }
        result
    }
}
