//! Keyboard simulation: send keystrokes and text to a window.

use anyhow::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

use crate::WinHandle;

/// Bring the window to the foreground and send a sequence of key events.
///
/// `keys` is a simple DSL:
/// - `{ENTER}` → VK_RETURN
/// - `{TAB}` → VK_TAB
/// - `{ESC}` → VK_ESCAPE
/// - Any other character → sent as Unicode via `KEYEVENTF_UNICODE`
pub fn send_keys(hwnd: WinHandle, keys: &str) -> Result<()> {
    let h = HWND(hwnd);
    unsafe {
        SetForegroundWindow(h);
    }
    std::thread::sleep(std::time::Duration::from_millis(50));

    let mut inputs: Vec<INPUT> = Vec::new();
    let mut chars = keys.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            for nc in chars.by_ref() {
                if nc == '}' {
                    break;
                }
                name.push(nc);
            }
            match name.to_uppercase().as_str() {
                "ENTER" | "RETURN" => push_vk(&mut inputs, VK_RETURN.0),
                "TAB" => push_vk(&mut inputs, 0x09),
                "ESCAPE" | "ESC" => push_vk(&mut inputs, 0x1B),
                _ => {}
            }
        } else {
            push_unicode_char(&mut inputs, c);
        }
    }

    if !inputs.is_empty() {
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
    Ok(())
}

/// Type arbitrary Unicode text into the foreground window.
pub fn send_text(hwnd: WinHandle, text: &str) -> Result<()> {
    let h = HWND(hwnd);
    unsafe {
        SetForegroundWindow(h);
    }
    std::thread::sleep(std::time::Duration::from_millis(50));

    let mut inputs: Vec<INPUT> = Vec::new();
    for c in text.chars() {
        push_unicode_char(&mut inputs, c);
    }
    if !inputs.is_empty() {
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
    Ok(())
}

fn push_vk(inputs: &mut Vec<INPUT>, vk: u16) {
    inputs.push(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: Default::default(),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
    inputs.push(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

fn push_unicode_char(inputs: &mut Vec<INPUT>, c: char) {
    let scan = c as u16;
    inputs.push(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: KEYEVENTF_UNICODE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
    inputs.push(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}
