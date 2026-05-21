//! GUI adapter for Cursor IDE.
//!
//! Integration strategy:
//! 1. Launch Cursor (or find existing window)
//! 2. Embed via Win32 SetParent into Synergy panel
//! 3. Send commands via UI Automation (find chat input → SetValue → invoke Send)
//! 4. Read responses via UI Automation (poll chat output element)
//! 5. Fallback: keyboard simulation if UIA elements not found

use crate::adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;

/// Default window title for Cursor IDE.
const CURSOR_WINDOW_TITLE: &str = "Cursor";

/// Default executable path (user can override via config).
#[cfg(windows)]
const CURSOR_DEFAULT_EXE: &str = "Cursor.exe";

pub struct CursorAdapter;

#[async_trait]
impl AppAdapter for CursorAdapter {
    fn id(&self) -> &str {
        "cursor"
    }

    fn display_name(&self) -> &str {
        "Cursor IDE"
    }

    fn app_type(&self) -> AppType {
        AppType::Gui
    }

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle> {
        #[cfg(not(windows))]
        {
            let _ = config;
            return Err(anyhow!("Cursor GUI adapter is Windows-only"));
        }

        #[cfg(windows)]
        {
            use synergy_win32::{launch_app, wait_for_window};

            let exe = if config.bin_path.is_empty() {
                CURSOR_DEFAULT_EXE
            } else {
                &config.bin_path
            };

            let mut args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
            if let Some(ref cwd) = config.cwd {
                args.push("--new-window");
                args.push(cwd);
            }

            let _pid = launch_app(exe, &args, config.cwd.as_deref())?;

            // Wait for the window to appear (up to 10 seconds)
            let hwnd = wait_for_window(CURSOR_WINDOW_TITLE, 10_000).await?;

            Ok(AppHandle {
                pty_session: None,
                window_hwnd: Some(hwnd as usize),
            })
        }
    }

    async fn send_command(&self, handle: &mut AppHandle, text: &str) -> Result<()> {
        #[cfg(not(windows))]
        {
            let _ = (handle, text);
            return Err(anyhow!("Windows-only"));
        }

        #[cfg(windows)]
        {
            let hwnd = handle
                .window_hwnd
                .ok_or_else(|| anyhow!("No window handle"))?;

            // Strategy 1: Try UI Automation
            if let Ok(()) = self.send_via_uia(hwnd as isize, text) {
                return Ok(());
            }

            // Strategy 2: Fallback to keyboard simulation
            self.send_via_keyboard(hwnd as isize, text)
        }
    }

    async fn read_output(&self, handle: &mut AppHandle) -> Option<String> {
        #[cfg(not(windows))]
        {
            let _ = handle;
            return None;
        }

        #[cfg(windows)]
        {
            let hwnd = handle.window_hwnd? as isize;
            self.read_via_uia(hwnd).ok()
        }
    }

    async fn detect_status(&self, _output_buffer: &str) -> AppStatus {
        // For GUI apps, status detection is based on UI element state
        // rather than text parsing. Default to Working; the orchestrator
        // will use a timeout or explicit "done" signal.
        AppStatus::Working
    }
}

#[cfg(windows)]
impl CursorAdapter {
    fn send_via_uia(&self, hwnd: isize, text: &str) -> Result<()> {
        use synergy_win32::UiAutomation;

        let uia = UiAutomation::new()?;
        let root = uia.element_from_handle(hwnd)?;

        // Cursor's chat input — try common automation IDs
        let input_el = uia
            .find_by_automation_id(&root, "chat-input")
            .or_else(|_| uia.find_by_name(&root, "Ask anything"))
            .or_else(|_| uia.find_by_name(&root, "Chat"))?;

        uia.set_value(&input_el, text)?;

        // Find and invoke the send button
        let send_btn = uia
            .find_by_name(&root, "Send")
            .or_else(|_| uia.find_by_name(&root, "Submit"))?;
        uia.invoke(&send_btn)?;

        Ok(())
    }

    fn send_via_keyboard(&self, hwnd: isize, text: &str) -> Result<()> {
        use synergy_win32::{send_keys, send_text};

        // Focus the chat input (Ctrl+L is common in Cursor)
        send_keys(hwnd, "\x0C")?; // Ctrl+L
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Type the text
        send_text(hwnd, text)?;
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Press Enter to send
        send_keys(hwnd, "{ENTER}")?;

        Ok(())
    }

    fn read_via_uia(&self, hwnd: isize) -> Result<String> {
        use synergy_win32::UiAutomation;

        let uia = UiAutomation::new()?;
        let root = uia.element_from_handle(hwnd)?;

        // Try to find the chat output/response area
        let output_el = uia
            .find_by_automation_id(&root, "chat-output")
            .or_else(|_| uia.find_by_name(&root, "Response"))
            .or_else(|_| uia.find_by_name(&root, "Chat messages"))?;

        uia.get_value(&output_el)
            .or_else(|_| uia.get_name(&output_el))
    }
}
