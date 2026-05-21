# Synergy User Guide

## Quick Start

### Prerequisites

- **Windows 10 21H2+** or **Windows 11** (x86_64)
- **OpenCode CLI** installed (for Workers): `npm i -g @anthropic-ai/opencode`
- **(Optional)** Proxy list for IP isolation
- **(Optional)** Cursor / Kiro for GUI Leader mode

### First Launch

1. Run `synergy-app.exe`
2. Fill in the setup form:
   - **Project Name**: your project identifier
   - **Database Path**: where session state is stored (default: `synergy.db`)
   - **Worker Adapter**: `opencode` (default) or `cli-generic` for plain shell
   - **Worker Count**: 1–12 (default: 6)
   - **Worker Binary**: path to the CLI tool (`opencode`, `cmd.exe`, etc.)
   - **Project Directory**: the folder Workers will operate in
3. Click **Launch Workspace**

### Dashboard Layout

```
┌─────────────────────────────────────────────────────────┐
│ PROGRESS │        WORKERS (terminals)       │  LEADER   │
│  (left)  │          (center)                │  (right)  │
│  ~15%    │          ~55%                    │  ~30%     │
└─────────────────────────────────────────────────────────┘
│                    STATUS BAR                            │
└─────────────────────────────────────────────────────────┘
```

- **Progress panel**: task checklist, progress bar, activity log
- **Workers panel**: live terminal output per worker (xterm.js)
- **Leader panel**: submit plans, add individual tasks

### Keyboard Shortcuts

| Shortcut | Action |
|---|---|
| `Ctrl+Shift+1` | Default layout (3 columns) |
| `Ctrl+Shift+2` | Focus Workers (full screen) |
| `Ctrl+Shift+3` | Focus Leader (full screen) |
| `Ctrl+Shift+4` | Minimal (Leader only) |
| `Ctrl+Shift+5` | Wide Workers (no Leader) |
| `Ctrl+1..6` | Focus Worker #1–6 input |
| `Ctrl+7` | Focus Leader plan textarea |
| `Escape` | Reset to default layout |

---

## Configuration

Config file: `%APPDATA%\Synergy\config.toml`

```toml
[general]
language = "id"
theme = "dark"
project_dir = "C:\\Users\\you\\Projects\\myapp"

[leader]
adapter = "opencode"    # or "cursor", "kiro", "api-direct"

[leader.api]            # only if adapter = "api-direct"
provider = "anthropic"
model = "claude-sonnet-4-20250514"

[workers]
count = 6
adapter = "opencode"
bin_path = "opencode"

[proxy]
mode = "list"           # "list" | "rotating" | "none"

[[proxy.list]]
address = "socks5://user:pass@proxy1.example.com:1080"
label = "Proxy-A"

[[proxy.list]]
address = "socks5://user:pass@proxy2.example.com:1080"
label = "Proxy-B"

[task]
max_retries = 2
timeout_minutes = 10
escalate_to_leader = true
```

---

## Leader Modes

### CLI Mode (Phase 1)
Leader runs as a CLI tool (OpenCode, Aider, Claude CLI) in a PTY.
Synergy reads its output and sends commands via stdin.

### GUI Mode (Phase 2)
Leader is a GUI app (Cursor, Kiro) embedded in the Leader panel
via Win32 SetParent. Synergy controls it via UI Automation or
keyboard simulation.

### API Mode (Phase 3)
Leader is a direct LLM API call. Synergy renders its own chat UI
and calls Anthropic/OpenAI/Google/local APIs directly.

---

## Task Workflow

1. User sends instruction to Leader (or types directly in plan textarea)
2. Leader produces a numbered plan
3. Synergy parses the plan into tasks with dependencies
4. Tasks are assigned to idle Workers (respecting file locks + deps)
5. Workers execute via PTY; output streams to terminal panels
6. When Worker returns to idle prompt → task marked Done
7. After all tasks complete → batch report sent to Leader for review
8. Leader approves or sends corrections
9. Corrections re-assigned to Workers
10. Session complete

---

## Git Integration

When enabled, Synergy auto-commits after each task:

```
[synergy] Task t_001 completed by Worker #2

Create auth module
```

Commands available via Tauri:
- `git_commit_task` — commit with task metadata
- `git_status` — show working tree status
- `git_log` — recent commit history

---

## Proxy Setup

Each Worker gets a unique proxy for IP isolation:

```toml
[proxy]
mode = "list"

[[proxy.list]]
address = "socks5://user1:pass@proxy1.example.com:1080"
label = "US-East"

[[proxy.list]]
address = "socks5://user2:pass@proxy2.example.com:1080"
label = "EU-West"
```

For rotating proxies (SmartProxy, BrightData):

```toml
[proxy]
mode = "rotating"
address = "socks5://user:pass@rotating.proxy.com:1080"
```

---

## Building from Source

```powershell
git clone https://github.com/<owner>/synergy.git
cd synergy
cargo build --workspace          # dev build
cargo test --workspace           # run all tests
cargo build --release            # optimized release
```

### Release build with installer:

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo wix --no-build --nocapture  # creates MSI in target/wix/
```

---

## Architecture

```
synergy-app          → Tauri entry point
synergy-ui           → Tauri commands + state management
synergy-core         → Orchestrator engine, task scheduler, git
synergy-adapter      → AppAdapter trait + CLI/GUI/API implementations
synergy-pty          → PTY multiplexer (ConPTY on Windows)
synergy-win32        → Win32 embedding, UI Automation, keyboard sim
synergy-proxy        → Proxy manager + health check
synergy-db           → SQLite state store
synergy-config       → TOML config loader
synergy-proto        → Shared types (Task, Message, Event)
```

---

## Troubleshooting

| Issue | Solution |
|---|---|
| "pipe is being closed" | PTY child exited. Check binary path exists. |
| Worker stuck on "Working" | Increase timeout or check if prompt regex matches your shell. |
| Proxy health check fails | Verify proxy address and credentials in config.toml. |
| GUI embed doesn't work | Ensure target app is running. Try launching it first. |
| Build fails on tauri-build | Run `cargo clean` then rebuild. Ensure VS Build Tools installed. |
