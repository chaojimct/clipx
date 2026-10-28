//! clipx 输入增强 runtime 的底座 crate（Step 0）。
//!
//! 只放三样东西：按应用 quirks 策略表（[`quirks`]，纯字符串逻辑，零平台依赖）、
//! 窄腰数据模型（[`model`]：Item / Action / Query / Trigger）、平台能力 trait
//! 骨架（[`platform`]：KeySource / Paste / Popup / Dialog，Windows 实现即现有
//! 代码平移，非 Windows 先 stub）。详见 `docs/runtime-design.md`。
//!
//! 纪律：本 crate 不依赖 UI、不依赖平台 crate、不新增常驻依赖（常驻 +0）；
//! trait 在 runtime Step 4 完成前只 workspace 内可见，对外只承诺 JSON 配置兼容。

pub mod model;
pub mod platform;
pub mod quirks;
