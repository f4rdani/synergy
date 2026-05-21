//! Window embedding: SetParent, resize, detach.

use anyhow::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetParent, SetWindowLongPtrW, SetWindowPos, GWL_STYLE,
    HWND_TOP, SWP_FRAMECHANGED, SWP_SHOWWINDOW, WS_CHILD,
};

use crate::WinHandle;

/// Reparent `child` into `parent_panel`, making it a child window that
/// fills the given dimensions.
pub fn embed_window(child: WinHandle, parent_panel: WinHandle, width: i32, height: i32) -> Result<()> {
    let child_hwnd = HWND(child);
    let parent_hwnd = HWND(parent_panel);
    unsafe {
        let style = GetWindowLongPtrW(child_hwnd, GWL_STYLE);
        SetWindowLongPtrW(child_hwnd, GWL_STYLE, style | WS_CHILD.0 as isize);
        SetParent(child_hwnd, parent_hwnd);
        SetWindowPos(
            child_hwnd,
            HWND_TOP,
            0,
            0,
            width,
            height,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        ).ok()?;
    }
    Ok(())
}

/// Resize an already-embedded child window.
pub fn resize_embedded_window(child: WinHandle, width: i32, height: i32) -> Result<()> {
    let child_hwnd = HWND(child);
    unsafe {
        SetWindowPos(child_hwnd, HWND_TOP, 0, 0, width, height, SWP_SHOWWINDOW).ok()?;
    }
    Ok(())
}

/// Detach a child window back to the desktop (undo embedding).
pub fn detach_window(child: WinHandle) -> Result<()> {
    let child_hwnd = HWND(child);
    unsafe {
        let style = GetWindowLongPtrW(child_hwnd, GWL_STYLE);
        SetWindowLongPtrW(child_hwnd, GWL_STYLE, style & !(WS_CHILD.0 as isize));
        SetParent(child_hwnd, HWND(0));
    }
    Ok(())
}
