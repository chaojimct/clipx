//! FileJump 文件夹跳转（M5）：#32770 对话框检测 + 多管理器路径采集 + 注入调度。
//!
//! 基线为 WPF 版 v1.9.8（`../clipboard/FileJump/`）：
//! - `FileDialogJumpHelper.cs` → [`dialog`]
//! - `FileManagerPathCollector.cs` → [`collectors`]
//! - `ShellDialogDeepNavigate.cs` + `native/ShellNavigate/*.dll` → [`inject`]
//!
//! ADR-008：本 crate 仅 Windows 有实质实现；非 Windows 下编译为 stub，
//! API 返回空/不支持，工作区其他成员照常编译。

pub mod collectors;
pub mod custom;
pub mod dialog;
pub mod dock;
pub mod inject;
pub mod models;

pub use dialog::{DialogKind, classify_dialog};
pub use models::{Candidate, CandidateSource};
