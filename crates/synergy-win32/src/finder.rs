//! Window discovery: find windows by title or class name.

use anyhow::{anyhow, Result};
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

use crate::WinHandle;

/// Find a top-level window whose title matches exactly.
pub fn find_window_by_title(title: &str) -> Result<WinHandle> {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())) };
    if hwnd.0 == 0 {
        return Err(anyhow!("Window with title '{}' not found", title));
    }
    Ok(hwnd.0)
}

/// Find a top-level window by its window class name.
pub fn find_window_by_class(class: &str) -> Result<WinHandle> {
    let wide: Vec<u16> = class.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(PCWSTR(wide.as_ptr()), PCWSTR::null()) };
    if hwnd.0 == 0 {
        return Err(anyhow!("Window with class '{}' not found", class));
    }
    Ok(hwnd.0)
}

/// Poll for a window with the given title, retrying until `timeout_ms`
/// elapses. Returns the handle on success.
pub async fn wait_for_window(title: &str, timeout_ms: u64) -> Result<WinHandle> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(h) = find_window_by_title(title) {
            return Ok(h);
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "Timed out waiting for window '{}' after {}ms",
                title,
                timeout_ms
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
