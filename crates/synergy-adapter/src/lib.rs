pub mod adapter;
pub mod api_direct;
pub mod cli_antigravity;
pub mod cli_codex;
pub mod cli_generic;
pub mod cli_opencode;
pub mod gui_cursor;
pub mod gui_kiro;

pub use adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
pub use api_direct::DirectApiAdapter;
pub use cli_antigravity::AntigravityAdapter;
pub use cli_codex::CodexAdapter;
pub use cli_generic::GenericCliAdapter;
pub use cli_opencode::OpenCodeAdapter;
pub use gui_cursor::CursorAdapter;
pub use gui_kiro::KiroAdapter;
