# Synergy — Next Implementation Plan

> Dokumen ini ditulis untuk session Kiro berikutnya. Baca dulu sebelum coding.

---

## 1. Konteks Singkat

**Synergy** adalah desktop app (Tauri + Rust) yang menjadikan **OpenCode CLI** sebagai
**Leader AI** yang mengkoordinasikan **6 Worker AI** (juga OpenCode) untuk menyelesaikan
task pemrograman secara paralel.

- Repo: `https://github.com/f4rdani/synergy` branch `main`
- Commit terakhir: `ccdefee` (v0.7.0)
- Lokal path: `C:\laragon\laragon\react\synergy`
- Spec lengkap: `SYNERGY.md`

---

## 2. State Saat Ini (yang sudah jalan)

✅ **Headless chat mode** — Leader pakai `opencode run` (bukan TUI), output plain text di chat bubble.
✅ **Slash commands** — `/model`, `/help`, `/version`, `/stats`, `/session`, `/providers`, `/agents`.
✅ **System Logs** — View → System Logs (panel kanan), event `synergy-log` dari backend.
✅ **Plan detection** — frontend parse numbered list dari output Leader, tombol **Delegate** muncul.
✅ **OpenCode bundled** — binary di `crates/synergy-app/binaries/opencode-x86_64-pc-windows-msvc.exe`,
   dipakai via `find_opencode_binary()` (resolve ke `target/debug/opencode.exe` saat build).
✅ **Isolated config** — `OPENCODE_HOME = {project}/.synergy/opencode-home/` agar tidak baca config user.
✅ **Force free model** — `--model opencode/deepseek-v4-flash-free`.
✅ **Auto-update check** di startup (5s timeout, ada Skip button, background install).
✅ **Loading screen** dengan step-by-step progress.
✅ **Public IP fetch** untuk Leader + tiap Worker (via proxy).
✅ **PTY resize sync** (xterm `onResize` → `resize_leader_pty` Tauri command).
✅ **Restart button** — respawn OpenCode setelah `/exit`.

---

## 3. Masalah yang HARUS di-fix (urutan prioritas)

### 🔴 P0 — Critical (UX rusak total)

#### 3.1 Leader tidak di-briefing dengan benar
- **Gejala**: User chat ke Leader, Leader langsung execute (buat file, edit code) tanpa planning.
- **Root cause**: `--prompt` flag tidak efektif untuk `opencode run`. System prompt
  yang ada di `synergy_core::leader::leader_system_prompt()` tidak sampai ke Leader.
- **Fix**: Kirim briefing sebagai **message pertama** via `opencode run "..."`.
  Briefing harus jelas: "Kamu adalah Leader. Tugas kamu HANYA buat numbered plan.
  JANGAN buat file. JANGAN execute. Setelah plan, tunggu user approve."
- **Lokasi**: `crates/synergy-adapter/src/cli_opencode_run.rs` — di method `launch()`
  trigger first message setelah PTY ready, atau di `synergy-ui/src/lib.rs::choose_leader`
  setelah session siap.

#### 3.2 Leader tidak chat duluan
- **Gejala**: User buka workspace, panel kiri kosong. Tidak ada greeting dari Leader.
- **Fix**: Setelah briefing terkirim, response Leader muncul sebagai assistant message
  pertama: "Halo! Saya Leader AI. Saya punya 6 Workers yang bisa kerja paralel.
  Apa yang mau kamu bangun?"
- **Tergantung**: Fix 3.1 dulu.

#### 3.3 Leader execute sendiri (tidak pakai Workers)
- **Gejala**: User minta "buat halaman login", Leader langsung buat file sendiri.
- **Root cause**: `opencode run` mode by default punya tools (Bash, Edit, Write) aktif.
  Leader pakai tools itu untuk execute langsung.
- **Fix**: Pakai flag `--agent plan` atau `--agent build` (cek `opencode agent list`).
  Atau gunakan custom agent yang DISABLE tools selain "thinking/planning".
  Atau prompt eksplisit: "JANGAN gunakan Bash/Edit/Write tools. HANYA respond dengan plan."
- **Lokasi**: `crates/synergy-adapter/src/cli_opencode_run.rs` — `send_command()`,
  tambahkan args `--agent plan` (kalau agent itu ada).

#### 3.4 Workers tidak auto-delegate
- **Gejala**: Plan terdeteksi → tombol Delegate muncul → user harus klik manual.
- **Fix**: Auto-trigger `delegateTasksToWorkers()` saat plan baru terdeteksi
  DAN user sudah send pesan setelah plan (menandakan approval implisit).
  ATAU kasih countdown "Auto-delegate in 5s... [Cancel]".
- **Lokasi**: `crates/synergy-ui/dist/index.html` — `detectLeaderPlan()` function.

### 🟡 P1 — Important (UX kurang)

#### 3.5 Output pump tidak stop setelah `opencode run` selesai
- **Gejala**: Backend log: `[opencode-run] completed successfully` lalu masih ada
  `[leader-pump] emitted N bytes` ratusan kali setelahnya.
- **Root cause**: `OpenCodeRunAdapter::read_output()` baca dari channel `output_rx`.
  Channel masih ada residual data dari banner / startup. Pump terus poll padahal
  data sudah habis.
- **Fix**: 
  - Track state `is_running: bool` di `OpenCodeRunState`. Set `true` saat send_command,
    `false` saat child exit. Pump only emit kalau `is_running` atau channel ada data.
  - Atau buang channel approach, langsung emit dari spawn task ke Tauri event.
- **Lokasi**: `crates/synergy-adapter/src/cli_opencode_run.rs`.

#### 3.6 Tidak ada status indicator
- **Gejala**: User tidak tahu Leader thinking, Workers working, atau idle.
- **Fix**: Status badge di header workspace:
  - `🟢 Idle — Ready for input`
  - `🟡 Leader thinking...`
  - `🔵 Workers executing (3/6)`
  - `🟣 Reviewing results`
  - `✅ Complete`
- **State sources**:
  - Leader status: dari `is_running` di `OpenCodeRunState`
  - Workers status: dari `Orchestrator.workers[].status` (sudah ada)
- **Lokasi**: `crates/synergy-ui/dist/index.html` — `renderWorkspace()` header,
  + polling `get_state` (sudah ada interval 1.5s).

#### 3.7 Tidak ada changelog
- **Gejala**: Workers bikin/edit file, user tidak tahu apa yang berubah.
- **Fix**: Setelah Workers done, run `git diff --stat` di project dir, tampilkan:
  ```
  📝 Changes (5 files):
  + database/migrations/2024_users.php (new)
  + app/Models/User.php (new)
  M routes/web.php (modified, +12 lines)
  ...
  ```
- **Backend**: `synergy_core::git::GitOps::diff_stat()` sudah ada.
- **Frontend**: tambah panel "Changes" di sidebar atau bottom.
- **Lokasi**: `crates/synergy-ui/src/lib.rs` — tambah Tauri command `get_git_changes`,
  + UI di `dist/index.html`.

### 🟢 P2 — Nice to have

#### 3.8 Auto-cancel button saat Leader thinking lama
- Jika Leader thinking >30s, tampilkan "Cancel" button.

#### 3.9 Worker output di mini-terminals
- Saat ini mini-terminals di kanan tidak menampilkan output Worker actual.
- Listen `worker-output` event sudah wired tapi mungkin Worker belum spawn properly.

---

## 4. Arsitektur Target

```
┌─────────────────────────────────────────────────────────────────┐
│  Synergy Window                                                  │
├─────────────────────────────────────────────────────────────────┤
│  File · Edit · View · Terminal · Help                            │
├─────────────────────────────────────────────────────────────────┤
│  🟢 LEADER (opencode plan-only)  IP: 1.2.3.4  │ STATUS: Idle    │
│  ┌───────────────────────────────────────┬──────────────────────┤
│  │ Chat (Leader)                         │ Workers (6)           │
│  │                                       │ ┌──────────────────┐ │
│  │ AI: Halo! Saya Leader. Punya 6        │ │ W1 idle  IP:5.5.5│ │
│  │     Workers paralel. Mau bangun apa?  │ │                  │ │
│  │                                       │ ├──────────────────┤ │
│  │ User: Buatkan halaman login PHP       │ │ W2 idle  IP:6.6.6│ │
│  │                                       │ │                  │ │
│  │ AI: Plan saya:                        │ ├──────────────────┤ │
│  │     1. Create User model — Files: ... │ │ W3 working IP:.. │ │
│  │     2. Create LoginController — ...   │ │ > Editing User..│ │
│  │     3. Create login.php view — ...    │ ├──────────────────┤ │
│  │                                       │ │ W4 working ...   │ │
│  │ [Auto-delegating in 5s... Cancel]     │ │                  │ │
│  │                                       │ ...                  │
│  ├───────────────────────────────────────┼──────────────────────┤
│  │ [Type message or /command...]   [Send]│ Changes (3 files)    │
│  │                                       │ + User.php (new)     │
│  │                                       │ + LoginCtrl.php (new)│
│  │                                       │ M routes.php (mod)   │
├──┴───────────────────────────────────────┴──────────────────────┤
│ Synergy v0.8.0  📁 myproject  Leader: opencode  Workers: 3/6 busy│
└─────────────────────────────────────────────────────────────────┘
```

**Flow:**

1. **Boot**: Loading screen → check update → spawn OpenCode dengan briefing.
2. **Briefing**: Leader auto-respond greeting setelah briefing.
3. **User input**: User chat "buatkan X".
4. **Leader plans**: OpenCode respond numbered plan (DENGAN tools disabled).
5. **Auto-delegate**: 5s countdown, lalu auto-spawn Workers + dispatch tasks.
6. **Workers execute**: Tiap Worker run `opencode run --continue "task..."` di project dir.
7. **Status updates**: Header badge update real-time (thinking/working/done).
8. **Changelog**: Setelah semua Worker done, run `git diff --stat`, tampilkan.
9. **Leader review**: Auto-send batch report ke Leader untuk review.
10. **Loop atau done**: Leader approve atau request fix.

---

## 5. File yang Harus di-touch

### Backend (Rust)
- `crates/synergy-adapter/src/cli_opencode_run.rs` — fix briefing, plan-only mode, pump stop logic
- `crates/synergy-ui/src/lib.rs` — tambah commands: `auto_delegate`, `get_git_changes`, `cancel_leader`
- `crates/synergy-core/src/leader.rs` — update `leader_system_prompt()` dengan instruksi tools-disabled
- `crates/synergy-core/src/lib.rs` — orchestrator: pastikan worker spawn pakai `opencode run --continue`

### Frontend
- `crates/synergy-ui/dist/index.html`:
  - Status badge di header
  - Auto-delegate dengan countdown
  - Changelog panel
  - Better Worker mini-terminal output

---

## 6. Test Plan

Setelah implementasi selesai, test dengan:

1. **Boot**: Buka app → loading screen → workspace muncul dengan greeting Leader.
2. **Slash command**: Ketik `/model` → list models muncul.
3. **Simple chat**: "hello" → Leader respond ramah TANPA buat file.
4. **Planning request**: "buatkan halaman login dengan PHP dan MySQL".
   - Leader respond dengan numbered plan.
   - Status: `🟡 Leader thinking...` saat respond.
   - Setelah plan terdeteksi: countdown 5s.
   - Auto-delegate trigger.
   - Status: `🔵 Workers executing (N/6)`.
   - Worker mini-terminals tampilkan output.
5. **Done**: Status: `✅ Complete`. Changelog panel muncul: "+ login.php (new)", dll.
6. **Verify file**: Cek file di project dir benar-benar dibuat.
7. **Restart**: Klik Restart → OpenCode respawn fresh dengan briefing ulang.
8. **Slash /exit**: Ketik `/exit` → OpenCode keluar → klik Restart → respawn.

---

## 7. Catatan Teknis

### OpenCode binary location
- Bundled: `crates/synergy-app/binaries/opencode-x86_64-pc-windows-msvc.exe`
- Saat build, Tauri copy ke: `target/debug/opencode.exe` (sibling synergy-app.exe)
- Function: `synergy_adapter::find_opencode_binary()` cari ini dulu, fallback ke PATH.

### OpenCode flags yang relevan
- `run "<message>"` — one-shot, exits after response.
- `--continue` — continue last session (persistent context).
- `--model opencode/deepseek-v4-flash-free` — force free model.
- `--pure` — disable plugins.
- `--agent <name>` — pakai specific agent (cek `opencode agent list`).
- `--prompt "<text>"` — system prompt (works for TUI, not sure for run mode).
- `--format json` — JSON output (lebih structured tapi belum tested di Synergy).

### Tauri events yang di-emit backend
- `leader-output` — chunk dari OpenCode stdout
- `worker-output` — chunk dari Worker PTY
- `leader-connection-status` — connect/disconnect/error
- `synergy-log` — system logs (level, source, message)
- `session-state-changed` — phase transitions

### Tauri commands yang sudah ada
- `select_folder`, `choose_leader`, `send_to_leader`, `send_raw_to_leader`
- `approve_plan`, `restart_leader`, `resize_leader_pty`
- `check_opencode_update`, `run_opencode_command`
- `get_public_ip`, `get_leader_info`, `get_workers_proxy_info`
- `get_state`, `get_session_flow_state`
- `git_commit_task`, `git_status`, `git_log`

---

## 8. Versioning

Saat ini di v0.7.0. Setelah implementasi besar ini selesai → **v0.8.0**.
Update di:
- `crates/synergy-app/tauri.conf.json` (`version` field)
- `crates/synergy-ui/dist/index.html` (`<title>` dan statusbar text)

---

## 9. Quick Start untuk New Session

```
1. Baca PLAN.md (file ini)
2. Baca SYNERGY.md (spec utama)
3. Cek state: git log --oneline -5
4. Cek file kritis:
   - crates/synergy-adapter/src/cli_opencode_run.rs
   - crates/synergy-ui/src/lib.rs (cari `choose_leader`, `approve_plan`)
   - crates/synergy-ui/dist/index.html (cari `renderWorkspace`, `handleLeaderChat`)
5. Mulai dari fix #3.1 (briefing) — itu unblock semua yang lain.
6. Build & test setiap fix sebelum lanjut.
7. Commit per fix dengan message yang clear.
```

---

## 10. Kontak

User: f4rdani (https://github.com/f4rdani)
Project: https://github.com/f4rdani/synergy
