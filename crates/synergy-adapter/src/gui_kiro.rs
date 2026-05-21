//! GUI adapter for Kiro IDE.
//!
//! Same integration strategy as Cursor (Electron-based, similar UIA tree).
//! Differences are in window title, executable path, and element names.

use crate::adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;

const KIRO_WINDOW_TITLE: &str = "Kiro";

#[cfg(windows)]
const KIRO_DEFAULT_EXE: &str = "Kiro.exe";

pub struct KiroAdapter;

#[async_trait]
impl AppAdapter for KiroAdapter {
    fn id(&self) -> &str {
        "kiro"
    }

    fn display_name(&self) -> &str {
        "Kiro IDE"
    }

    fn app_type(&self) -> AppType {
        AppType::Gui
    }

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle> {
        #[cfg(not(windows))]
        {
            let _ = config;
            return Err(anyhow!("Kiro GUI adapter is Windows-only"));
        }

        #[cfg(windows)]
        {
            use synergy_win32::{launch_app, wait_for_window};

            let exe = if config.bin_path.is_empty() {
                KIRO_DEFAULT_EXE
            } else {
                &config.bin_path
            };

            let mut args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
            if let Some(ref cwd) = config.cwd {
                args.push("--new-window");
                args.push(cwd);
            }

            let _pid = launch_app(exe, &args, config.cwd.as_deref())?;

            let hwnd = wait_for_window(KIRO_WINDOW_TITLE, 10_000).await?;

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

            if let Ok(()) = self.send_via_uia(hwnd as isize, text) {
                return Ok(());
            }

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
        AppStatus::Working
    }
}

#[cfg(windows)]
impl KiroAdapter {
    fn send_via_uia(&self, hwnd: isize, text: &str) -> Result<()> {
        use synergy_win32::UiAutomation;

        let uia = UiAutomation::new()?;
        let root = uia.element_from_handle(hwnd)?;

        // Kiro chat input — try known patterns
        let input_el = uia
            .find_by_automation_id(&root, "chat-input")
            .or_else(|_| uia.find_by_name(&root, "Message Kiro"))
            .or_else(|_| uia.find_by_name(&root, "Ask Kiro"))?;

        uia.set_value(&input_el, text)?;

        let send_btn = uia
            .find_by_name(&root, "Send")
            .or_else(|_| uia.find_by_name(&root, "Submit"))
            .or_else(|_| uia.find_by_automation_id(&root, "send-button"))?;
        uia.invoke(&send_btn)?;

        Ok(())
    }

    fn send_via_keyboard(&self, hwnd: isize, text: &str) -> Result<()> {
        use synergy_win32::{send_keys, send_text};

        // Kiro: Ctrl+Shift+I or similar to focus chat
        send_keys(hwnd, "\x0C")?; // Ctrl+L as fallback
        std::thread::sleep(std::time::Duration::from_millis(200));
        send_text(hwnd, text)?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        send_keys(hwnd, "{ENTER}")?;
        Ok(())
    }

    fn read_via_uia(&self, hwnd: isize) -> Result<String> {
        use synergy_win32::UiAutomation;

        let uia = UiAutomation::new()?;
        let root = uia.element_from_handle(hwnd)?;

        let output_el = uia
            .find_by_automation_id(&root, "chat-output")
            .or_else(|_| uia.find_by_name(&root, "Response"))
            .or_else(|_| uia.find_by_name(&root, "Chat messages"))?;

        uia.get_value(&output_el)
            .or_else(|_| uia.get_name(&output_el))
    }
}
