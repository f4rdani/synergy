//! Win32 integration layer for Synergy Phase 2.
//!
//! Provides:
//! - Window discovery (`find_window_by_title`, `find_window_by_class`)
//! - Window embedding (`embed_window`, `resize_embedded_window`, `detach_window`)
//! - Process launching (`launch_app`)
//! - UI Automation (`UiAutomation` — find elements, set value, invoke, read text)
//! - Keyboard simulation (`send_keys`, `send_text`)
//!
//! All public functions are safe wrappers around unsafe Win32 calls.
//! On non-Windows platforms, stubs return errors.

#[cfg(windows)]
pub mod embedding;
#[cfg(windows)]
pub mod finder;
#[cfg(windows)]
pub mod keyboard;
#[cfg(windows)]
pub mod launcher;
#[cfg(windows)]
pub mod uia;

// Re-exports for convenience
#[cfg(windows)]
pub use embedding::{detach_window, embed_window, resize_embedded_window};
#[cfg(windows)]
pub use finder::{find_window_by_class, find_window_by_title, wait_for_window};
#[cfg(windows)]
pub use keyboard::{send_keys, send_text};
#[cfg(windows)]
pub use launcher::launch_app;
#[cfg(windows)]
pub use uia::UiAutomation;

/// Opaque window handle (HWND as isize for cross-module use).
pub type WinHandle = isize;

// ─── Non-Windows stubs ───────────────────────────────────────────────────────

#[cfg(not(windows))]
pub fn embed_window(_child: WinHandle, _parent: WinHandle, _w: i32, _h: i32) -> anyhow::Result<()> {
    anyhow::bail!("Windows-only feature")
}
#[cfg(not(windows))]
pub fn resize_embedded_window(_child: WinHandle, _w: i32, _h: i32) -> anyhow::Result<()> {
    anyhow::bail!("Windows-only feature")
}
#[cfg(not(windows))]
pub fn detach_window(_child: WinHandle) -> anyhow::Result<()> {
    anyhow::bail!("Windows-only feature")
}
#[cfg(not(windows))]
pub fn find_window_by_title(_title: &str) -> anyhow::Result<WinHandle> {
    anyhow::bail!("Windows-only feature")
}
#[cfg(not(windows))]
pub fn find_window_by_class(_class: &str) -> anyhow::Result<WinHandle> {
    anyhow::bail!("Windows-only feature")
}
#[cfg(not(windows))]
pub async fn wait_for_window(_title: &str, _timeout_ms: u64) -> anyhow::Result<WinHandle> {
    anyhow::bail!("Windows-only feature")
}
#[cfg(not(windows))]
pub fn launch_app(_exe: &str, _args: &[&str], _cwd: Option<&str>) -> anyhow::Result<u32> {
    anyhow::bail!("Windows-only feature")
}
#[cfg(not(windows))]
pub fn send_keys(_hwnd: WinHandle, _keys: &str) -> anyhow::Result<()> {
    anyhow::bail!("Windows-only feature")
}
#[cfg(not(windows))]
pub fn send_text(_hwnd: WinHandle, _text: &str) -> anyhow::Result<()> {
    anyhow::bail!("Windows-only feature")
}
