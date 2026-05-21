# Synergy — Multi-App AI Workspace Orchestrator

Synergy is a desktop application workspace designed to combine multiple AI coding assistants (both CLI and GUI apps) into a single, unified environment. It uses a **Leader–Worker** architecture, where a designated "Leader" AI orchestrates and distributes tasks to multiple parallel "Worker" instances.

## Core Features

- **Multi-App Hosting:** Embeds both CLI tools (via PTY) and GUI apps (via Win32 embedding) in a unified screen layout.
- **Leader Flexibility:** Set any AI engine (Kiro, Cursor, Aider, direct LLM API, local models) as the master orchestrator.
- **Worker Isolation:** Parallel worker pools (defaulting to 6 instances of OpenCode CLI) isolated via separate proxy IP addresses to bypass API and IP-based rate limits.
- **Unified Activity Log:** Real-time visibility of active processes, task status, execution logs, and automated file-conflict locking.

For details on the architecture, see [docs/SYNERGY.md](docs/SYNERGY.md) (or root `SYNERGY.md`).
