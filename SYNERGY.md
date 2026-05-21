<!-- markdownlint-disable MD013 MD033 MD024 -->

# Synergy — Multi-App AI Workspace Orchestrator

> Nama kerja: **Synergy**. Dokumen ini adalah cetak biru (blueprint)
> end-to-end untuk membangun aplikasi desktop berbasis **Rust** yang
> menggabungkan banyak AI tool (CLI & GUI) ke dalam satu workspace
> terpadu, dengan arsitektur **Leader–Worker** di mana satu AI
> mengorkestrasikan banyak AI lain secara paralel.
>
> **Target platform utama:** **Windows 10 / 11 (x86_64)**.
>
> **Konsep inti:** User punya berbagai langganan/akses AI tool
> (Kiro, Cursor, Antigravity, Codex, API key, dll). Synergy
> menyatukan semuanya dalam satu layar — Leader mengkoordinasi,
> Worker mengeksekusi. Setiap tool tetap berjalan sebagai
> aplikasi aslinya, bukan "dipinjam API-nya".

---

## Daftar Isi

1. [Visi & Tujuan](#1-visi--tujuan)
2. [Glosarium](#2-glosarium)
3. [Pilar Desain (NFR)](#3-pilar-desain-nfr)
4. [Arsitektur Tingkat Tinggi](#4-arsitektur-tingkat-tinggi)
5. [Leader: Orkestrator Utama](#5-leader-orkestrator-utama)
6. [Worker: OpenCode CLI × 6](#6-worker-opencode-cli--6)
7. [Proxy Manager (IP Isolation)](#7-proxy-manager-ip-isolation)
8. [Dua Mode Integrasi: CLI vs GUI](#8-dua-mode-integrasi-cli-vs-gui)
9. [CLI Integration via PTY](#9-cli-integration-via-pty)
10. [GUI Integration via Win32 Embedding](#10-gui-integration-via-win32-embedding)
11. [UI Automation Framework](#11-ui-automation-framework)
12. [Antarmuka Pengguna (UI/UX)](#12-antarmuka-pengguna-uiux)
13. [App Adapter System](#13-app-adapter-system)
14. [Supported Apps (Leader)](#14-supported-apps-leader)
15. [Protokol Komunikasi Leader–Worker](#15-protokol-komunikasi-leaderworker)
16. [Task Management & Orchestration](#16-task-management--orchestration)
17. [Skema Data & Penyimpanan](#17-skema-data--penyimpanan)
18. [Tech Stack & Dependensi Rust](#18-tech-stack--dependensi-rust)
19. [Struktur Repository](#19-struktur-repository)
20. [Build, Install, Run](#20-build-install-run)
21. [Roadmap & Milestone](#21-roadmap--milestone)
22. [Risiko & Mitigasi](#22-risiko--mitigasi)
23. [Pertanyaan Terbuka](#23-pertanyaan-terbuka)

---

## 1. Visi & Tujuan

### 1.1 Visi Singkat

Sebuah workspace desktop tunggal di mana developer:

1. Memilih **Leader** dari AI tool apa pun yang sudah mereka miliki
   (Kiro, Cursor, Antigravity, Codex, OpenCode, API key langsung,
   atau CLI tool lainnya).
2. Melihat **6 Worker** (OpenCode CLI, masing-masing dengan IP
   berbeda via proxy) mengeksekusi tugas secara paralel di panel
   terpisah.
3. Menikmati satu layar terpadu — mirip multi-window / tiling
   window manager — di mana Leader bisa mengakses, membaca, dan
   mengirim perintah ke semua Worker.

### 1.2 Poin Penjualan Utama (Unique Selling Points)

- **USP-1 — Integrasi lintas-aplikasi**: Synergy bisa meng-host
  aplikasi AI yang berbeda-beda (CLI & GUI) dalam satu workspace.
  Tidak peduli apakah user pakai Kiro, Cursor, atau terminal —
  semuanya bisa dikelola dari satu tempat.
- **USP-2 — Leader bebas pilih**: User pakai apa pun model/tool
  terkuat yang mereka punya. Sudah langganan Kiro? Pakai sebagai
  Leader. Punya API key Anthropic? Juga bisa. Tidak ada lock-in.
- **USP-3 — Worker gratis × 6**: Semua Worker menggunakan OpenCode
  (CLI, free models built-in). Masing-masing Worker punya IP
  berbeda via proxy, sehingga rate limit terisolasi per-Worker.
  Efektif = free tier × 6.
- **USP-4 — Tampilan unified**: Satu layar, banyak panel. User
  melihat semua aktivitas Leader dan Worker secara real-time,
  seperti command center.

### 1.3 Tujuan Produk

- **G1 — Multi-app hosting**: Bisa embed/jalankan app CLI dan GUI
  dalam satu window Synergy.
- **G2 — Leader flexibility**: Leader bisa pakai app/tool/API apa
  pun — konfigurasi di settings, langsung jalan.
- **G3 — Worker isolation**: 6 Worker OpenCode CLI, masing-masing
  berjalan di proxy berbeda, rate limit independen.
- **G4 — Visual orchestration**: User melihat task progress di
  setiap panel Worker secara real-time.
- **G5 — Cross-app coordination**: Leader bisa membaca output Worker
  dan mengirim instruksi lanjutan — otomatis atau manual.

### 1.4 Non-Goals (v1)

- Bukan IDE penuh (tidak menggantikan VS Code / Cursor).
- Bukan cloud SaaS (desktop single-user only).
- Tidak menyediakan model AI sendiri — semua kecerdasan berasal
  dari app/tool yang di-host.
- Tidak menyediakan proxy/VPN — user menyediakan sendiri daftar
  proxy, Synergy hanya menggunakannya.

---

## 2. Glosarium

| Istilah | Definisi |
| --- | --- |
| **Leader** | AI tool utama yang dipilih user sebagai orkestrator. Bisa app GUI (Kiro, Cursor), CLI (OpenCode, Aider), atau API key langsung. |
| **Worker** | Instance OpenCode CLI yang menjalankan tugas. Default 6 unit, masing-masing di-proxy ke IP berbeda. |
| **App Adapter** | Modul yang tahu cara meng-embed, mengontrol, dan membaca output dari satu jenis app tertentu. |
| **PTY** | Pseudo-terminal — cara mengontrol app CLI via stdin/stdout secara programatik. |
| **Window Embedding** | Teknik Win32 (`SetParent`) untuk memasukkan window aplikasi lain ke dalam panel Synergy. |
| **UI Automation** | API Windows untuk mengontrol elemen UI app lain secara programatik (klik, ketik, baca teks). |
| **Proxy** | HTTP/SOCKS5 proxy yang diberikan ke setiap Worker agar masing-masing punya IP unik. |
| **IP Isolation** | Setiap Worker melewati proxy berbeda sehingga rate limit (yang berbasis IP) terisolasi per-Worker. |
| **Task** | Satu unit pekerjaan yang didelegasikan Leader ke Worker (misal: "buat file auth.ts"). |
| **Panel** | Satu area di UI Synergy yang menampilkan satu app (Leader atau Worker). |

---

## 3. Pilar Desain (NFR)

| Kode | Pilar | Target |
| --- | --- | --- |
| NFR-1 | **App Agnostic** | Leader bisa pakai app apa pun — Synergy tidak terikat ke satu provider/tool. |
| NFR-2 | **Zero-cost Workers** | Worker default (OpenCode CLI) tidak memerlukan API key berbayar. |
| NFR-3 | **IP Isolation** | Setiap Worker punya IP sendiri via proxy. Rate limit satu Worker tidak memengaruhi Worker lain. |
| NFR-4 | **Recoverability** | Crash Synergy tidak menghentikan app yang di-embed. Sesi bisa dilanjutkan. |
| NFR-5 | **Minimal setup** | User hanya perlu: (1) pilih Leader app, (2) sediakan daftar proxy, (3) klik Start. |
| NFR-6 | **Observability** | Semua task dan output tercatat. User bisa replay aktivitas agen. |
| NFR-7 | **Windows-first** | Semua fitur Win32 (SetParent, UI Automation, proxy per-process) dioptimasi untuk Windows. |

---

## 4. Arsitektur Tingkat Tinggi

```text
+---------------------------------------------------------------+
|                     Synergy Host App (Rust / Tauri)            |
|                                                                |
|  +------------------+  +-------------+  +-------------+       |
|  |   LEADER PANEL   |  | WORKER 1    |  | WORKER 2    |       |
|  |                  |  | OpenCode    |  | OpenCode    |       |
|  | (Kiro / Cursor / |  | CLI via PTY |  | CLI via PTY |       |
|  |  API / Terminal) |  | Proxy: IP-A |  | Proxy: IP-B |       |
|  +------------------+  +-------------+  +-------------+       |
|                                                                |
|  +-------------+  +-------------+  +-------------+            |
|  | WORKER 3    |  | WORKER 4    |  | WORKER 5    |            |
|  | OpenCode    |  | OpenCode    |  | OpenCode    |            |
|  | CLI via PTY |  | CLI via PTY |  | CLI via PTY |            |
|  | Proxy: IP-C |  | Proxy: IP-D |  | Proxy: IP-E |            |
|  +-------------+  +-------------+  +-------------+            |
|                                                                |
|  +-------------+                                               |
|  | WORKER 6    |     +-------------------------------------+   |
|  | OpenCode    |     | Orchestrator Engine                 |   |
|  | CLI via PTY |     |  - Task queue & distribution        |   |
|  | Proxy: IP-F |     |  - Leader ↔ Worker communication    |   |
|  +-------------+     |  - Output monitoring & parsing      |   |
|                      |  - Proxy manager                    |   |
|                      +-------------------------------------+   |
|                                                                |
|  [Status Bar: Leader: Kiro | Workers: 4 busy / 2 idle]        |
+---------------------------------------------------------------+
        |              |              |             |
        v              v              v             v
   Leader App     OpenCode ×6     Proxy Pool     File System
   (embedded/     (PTY procs)    (SOCKS5/HTTP)   (project dir)
    spawned)
```

Tiga lapisan:

1. **UI Layer** — render panel, status, dan kontrol user.
2. **Orchestrator Engine** — manajemen task, komunikasi Leader↔Worker,
   monitoring output, proxy assignment.
3. **External Resources** — app yang di-embed/spawn, proxy pool,
   file system proyek.

---

## 5. Leader: Orkestrator Utama

### 5.1 Peran Leader

1. Menerima instruksi dari user (misal: "Buat fitur login").
2. Memecah instruksi menjadi sub-task.
3. Mendistribusikan sub-task ke Worker.
4. Memantau progress setiap Worker.
5. Melakukan koreksi jika Worker gagal.

### 5.2 Leader Bisa Pakai Apa Saja

User memilih Leader berdasarkan tool/model terkuat yang mereka punya:

| Kategori | Contoh App | Cara Integrasi |
| --- | --- | --- |
| **GUI App** | Kiro, Cursor, VS Code + Copilot, Windsurf | Win32 SetParent (embed window) + UI Automation |
| **CLI Tool** | OpenCode, Aider, Claude CLI, Codex CLI | PTY (stdin/stdout) |
| **API Key** | Anthropic, OpenAI, Google Gemini, DeepSeek | HTTP client langsung (reqwest) |
| **Lokal** | Ollama, LM Studio | HTTP ke localhost |

### 5.3 Leader Mode

```text
User memilih di Settings:

  ┌─ "GUI App" ──→ Synergy embed window app ke Leader Panel
  │                 + kontrol via UI Automation
  │
  ├─ "CLI Tool" ─→ Synergy spawn PTY di Leader Panel
  │                 + kontrol via stdin/stdout
  │
  └─ "API Key" ──→ Synergy tampilkan chat UI sendiri di Leader Panel
                    + panggil API langsung via HTTP
```

### 5.4 Kenapa Leader Bebas?

Setiap user punya situasi berbeda:

- User A: sudah bayar langganan Kiro → pakai Kiro sebagai Leader,
  sayang kalau tidak dipakai.
- User B: punya API key Anthropic Claude Opus → pakai API langsung.
- User C: laptop kuat, punya Ollama lokal → pakai Ollama.
- User D: pakai Cursor Pro → embed Cursor sebagai Leader.

Synergy tidak memaksa user pindah ke satu ekosistem.

---

## 6. Worker: OpenCode CLI × 6

### 6.1 Kenapa OpenCode?

1. **CLI-based** — mudah dikontrol via PTY (stdin/stdout). Paling
   stabil untuk automasi.
2. **Free models built-in** — OpenCode sudah include akses ke model
   gratis (tanpa API key berbayar).
3. **Rate limit berbasis IP** — dengan proxy berbeda per Worker,
   setiap Worker mendapat kuota rate limit sendiri.
4. **Open source** — bisa diaudit, tidak ada vendor lock-in.

### 6.2 Anatomi Worker

```text
+----------------------- Worker N -------------------------+
|                                                          |
|  ┌─ Environment ──────────────────────────────────────┐  |
|  │  HTTP_PROXY  = socks5://proxy-n:port               │  |
|  │  HTTPS_PROXY = socks5://proxy-n:port               │  |
|  │  CWD         = /path/to/project                    │  |
|  └────────────────────────────────────────────────────┘  |
|                                                          |
|  ┌─ PTY Process ──────────────────────────────────────┐  |
|  │  $ opencode                                        │  |
|  │                                                    │  |
|  │  stdin  ← Synergy mengirim task/perintah           │  |
|  │  stdout → Synergy membaca output/response          │  |
|  │  stderr → Synergy menangkap error                  │  |
|  └────────────────────────────────────────────────────┘  |
|                                                          |
|  ┌─ Panel UI ─────────────────────────────────────────┐  |
|  │  Menampilkan terminal output secara real-time       │  |
|  │  Badge status: idle / working / error / done        │  |
|  └────────────────────────────────────────────────────┘  |
+----------------------------------------------------------+
```

### 6.3 Worker Pool

| Setting | Default | Range |
| --- | --- | --- |
| `max_workers` | 6 | 1–12 |
| `worker_app` | `opencode` | Bisa diganti CLI lain |
| `project_dir` | Dipilih user | Semua Worker share folder yang sama |
| `auto_restart` | `true` | Restart Worker jika crash |

### 6.4 Task Assignment

Ketika Leader memecah instruksi menjadi 4 sub-task:

```text
Leader: "Buat fitur login"
  │
  ├─ Task 1: "Buat migration tabel users"      → Worker 1
  ├─ Task 2: "Buat model User dan controller"   → Worker 2
  ├─ Task 3: "Buat halaman login (blade/vue)"   → Worker 3
  └─ Task 4: "Buat unit test auth"              → Worker 4
  
  Worker 5 & 6: idle (standby untuk task baru / retry)
```

Task assignment mengikuti aturan:
- Task independen → paralel ke Worker berbeda.
- Task dependen (perlu output task lain) → antri sampai dependency selesai.
- Worker yang gagal → task di-reassign ke Worker idle.

### 6.5 Concurrency & File Conflict

Semua Worker mengerjakan folder proyek yang sama. Strategi menghindari
konflik:

1. **Task-level locking**: setiap task mendeklarasikan file yang akan
   disentuh. Synergy mencegah 2 Worker mengedit file yang sama
   bersamaan.
2. **Sequential fallback**: jika task saling dependen, dijalankan
   berurutan.
3. **Git-based conflict detection**: setelah task selesai, Synergy
   jalankan `git diff` untuk mendeteksi konflik. Jika ada → eskalasi
   ke Leader untuk resolusi.

---

## 7. Proxy Manager (IP Isolation)

### 7.1 Tujuan

OpenCode (dan banyak free-tier AI service) memberlakukan rate limit
berbasis IP. Jika semua Worker jalan dari IP yang sama, rate limit
shared → cepat habis. Dengan proxy berbeda per Worker, setiap Worker
mendapat jatah rate limit sendiri.

```text
                   Internet
                      │
        ┌─────────────┼─────────────┐
        │             │             │
   ┌────▼───┐   ┌────▼───┐   ┌────▼───┐
   │Proxy A │   │Proxy B │   │Proxy C │   ...
   │IP: 1.1 │   │IP: 2.2 │   │IP: 3.3 │
   └────┬───┘   └────┬───┘   └────┬───┘
        │             │             │
   ┌────▼───┐   ┌────▼───┐   ┌────▼───┐
   │Worker 1│   │Worker 2│   │Worker 3│   ...
   └────────┘   └────────┘   └────────┘
```

### 7.2 Konfigurasi Proxy

User menyediakan daftar proxy di `config.toml`:

```toml
[proxy]
mode = "list"   # "list" | "rotating" | "none"

[[proxy.list]]
address = "socks5://user1:pass@proxy1.example.com:1080"
label   = "Proxy-A"

[[proxy.list]]
address = "socks5://user2:pass@proxy2.example.com:1080"
label   = "Proxy-B"

[[proxy.list]]
address = "http://user3:pass@proxy3.example.com:8080"
label   = "Proxy-C"

[[proxy.list]]
address = "socks5://user4:pass@proxy4.example.com:1080"
label   = "Proxy-D"

[[proxy.list]]
address = "socks5://user5:pass@proxy5.example.com:1080"
label   = "Proxy-E"

[[proxy.list]]
address = "socks5://user6:pass@proxy6.example.com:1080"
label   = "Proxy-F"
```

Mode `rotating` = menggunakan satu proxy service yang otomatis rotasi IP
(misal: SmartProxy, BrightData, atau IPRoyal).

```toml
[proxy]
mode = "rotating"
address = "socks5://user:pass@rotating.proxy.com:1080"
```

Synergy akan tetap spawn 6 koneksi berbeda — proxy rotating service
akan memberikan IP berbeda per koneksi.

### 7.3 Assignment

```text
Saat startup:
  Worker 1 → $env:HTTP_PROXY = proxy.list[0].address
  Worker 2 → $env:HTTP_PROXY = proxy.list[1].address
  ...
  Worker 6 → $env:HTTP_PROXY = proxy.list[5].address
```

Proxy di-set sebagai environment variable pada proses PTY masing-masing
Worker. App OpenCode di dalam PTY tersebut akan otomatis menggunakan
proxy via environment variable standar `HTTP_PROXY` / `HTTPS_PROXY`.

### 7.4 Health Check

Synergy secara periodik mengecek kesehatan proxy:

```text
Setiap 60 detik:
  untuk setiap proxy:
    test koneksi TCP ke proxy address
    jika gagal:
      tandai proxy sebagai "unhealthy"
      re-assign Worker ke proxy healthy lain (jika tersedia)
      tampilkan warning di UI
```

### 7.5 Mode Tanpa Proxy

Jika user tidak mengkonfigurasi proxy:

- Semua Worker jalan dari IP yang sama (IP host).
- Rate limit shared — mungkin lebih lambat.
- Warning di UI: "Workers berbagi IP. Rate limit mungkin terpengaruh."
- Synergy tetap berfungsi penuh, hanya lebih lambat.

---

## 8. Dua Mode Integrasi: CLI vs GUI

Synergy mengintegrasikan app eksternal dengan dua cara:

### 8.1 Mode CLI (via PTY) — Stabil, Primary

```text
Synergy ──spawn──→ PTY process ──run──→ opencode / aider / dll
           │
           ├── stdin:  Synergy mengirim teks (perintah/prompt)
           ├── stdout: Synergy membaca output (response)
           └── stderr: Synergy menangkap error
```

Keunggulan:
- **Sangat stabil** — stdin/stdout tidak berubah antar versi app.
- **Mudah diimplementasi** — crate `portable-pty`.
- **Full control** — Synergy bisa mengirim apa pun, termasuk Ctrl+C.

Dipakai untuk: **semua Worker (OpenCode CLI)** dan **Leader CLI mode**.

### 8.2 Mode GUI (via Win32 Embedding) — Fitur Premium, Phase 2

```text
Synergy ──FindWindow──→ HWND app (Cursor/Kiro/dll)
           │
           ├── SetParent(hwnd, synergy_panel_hwnd)
           │   → window app "masuk" ke dalam panel Synergy
           │
           ├── UI Automation (UIA):
           │   → Find chat input element
           │   → SetValue("perintah dari Leader")
           │   → Invoke("Send button")
           │   → GetValue(response text)
           │
           └── SetWindowPos(hwnd, ...)
               → resize agar pas di panel
```

Keunggulan:
- **Fitur wow** — user melihat Cursor/Kiro berjalan di dalam Synergy.
- **Preserves full app** — semua fitur app tetap tersedia.

Tantangan:
- App update bisa mengubah UI element → adapter perlu di-update.
- Electron app (Cursor, Kiro) umumnya support UI Automation dengan baik.

Dipakai untuk: **Leader GUI mode** (Phase 2+).

---

## 9. CLI Integration via PTY

### 9.1 Arsitektur PTY

```text
                    Synergy Process
                         │
              ┌──────────┼──────────┐
              │          │          │
         ┌────▼──┐  ┌───▼───┐  ┌──▼────┐
         │ PTY 1 │  │ PTY 2 │  │ PTY N │
         │       │  │       │  │       │
         │opencode│  │opencode│  │opencode│
         └───────┘  └───────┘  └───────┘
```

### 9.2 Library: `portable-pty`

Cross-platform PTY:
- Windows: ConPTY (Windows 10 1809+).
- Mendukung ANSI escape parsing.
- Non-blocking read via tokio.

### 9.3 Alur Kirim Perintah ke Worker

```text
1. Leader memutuskan task: "Buat file src/auth/login.ts"
2. Synergy Orchestrator mengambil Worker idle (misal Worker 3)
3. Synergy menulis ke PTY stdin Worker 3:
   → "Create the file src/auth/login.ts with a login endpoint
      using Express.js and bcrypt for password hashing"
4. Worker 3 (OpenCode) memproses, menulis file
5. Synergy membaca PTY stdout Worker 3:
   → parsing output untuk mendeteksi "selesai" / "error"
6. Synergy melaporkan status ke UI + Leader
```

### 9.4 Output Parsing

Synergy perlu mem-parsing output terminal untuk mengetahui status
Worker:

| Signal | Artinya | Deteksi |
| --- | --- | --- |
| Prompt kembali muncul (misal `> `) | Worker idle, siap terima task baru | Regex pattern matching |
| Error message (stderr / exit code) | Task gagal | Pattern: `error`, `Error`, `failed`, exit code != 0 |
| File diff output | Worker selesai mengedit file | Pattern: `+++ b/`, `--- a/` |
| "Done" / completion message | Task selesai | Pattern dari output OpenCode |

Parser bersifat adapter-specific — tiap CLI app punya pola output
berbeda. Adapter OpenCode mendefinsikan pola-pola ini.

---

## 10. GUI Integration via Win32 Embedding

### 10.1 Teknik: SetParent

Win32 API `SetParent(child_hwnd, parent_hwnd)` memindahkan window
aplikasi lain menjadi child window Synergy.

```rust
use windows::Win32::UI::WindowsAndMessaging::{
    SetParent, SetWindowPos, FindWindowW, SetWindowLongPtrW,
    GetWindowLongPtrW, GWL_STYLE, WS_CHILD, HWND_TOP,
    SWP_FRAMECHANGED, SWP_SHOWWINDOW
};

fn embed_window(child: HWND, parent_panel: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(child, GWL_STYLE);
        SetWindowLongPtrW(child, GWL_STYLE, style | WS_CHILD as isize);
        SetParent(child, parent_panel);
        SetWindowPos(
            child, HWND_TOP,
            0, 0, panel_width, panel_height,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW
        );
    }
}
```

### 10.2 Finding Target Window

Untuk embed Cursor:

```rust
let hwnd = FindWindowW(None, w!("Cursor"));
```

Untuk embed Kiro:

```rust
let hwnd = FindWindowW(None, w!("Kiro"));
```

Jika app belum berjalan, Synergy bisa meluncurkannya:

```rust
Command::new("C:\\Users\\<user>\\AppData\\Local\\Programs\\cursor\\Cursor.exe")
    .arg("--new-window")
    .arg(project_path)
    .spawn()?;

tokio::time::sleep(Duration::from_secs(3)).await;
let hwnd = FindWindowW(None, w!("Cursor"));
```

### 10.3 Resize Handling

Saat user resize panel Synergy, embedded window harus ikut resize:

```rust
fn on_panel_resize(child: HWND, new_width: i32, new_height: i32) {
    unsafe {
        SetWindowPos(child, HWND_TOP, 0, 0, new_width, new_height, SWP_SHOWWINDOW);
    }
}
```

### 10.4 Detach

Saat Synergy ditutup atau user memilih "detach", window dikembalikan:

```rust
fn detach_window(child: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(child, GWL_STYLE);
        SetWindowLongPtrW(child, GWL_STYLE, style & !(WS_CHILD as isize));
        SetParent(child, HWND(0)); // kembali ke desktop
    }
}
```

---

## 11. UI Automation Framework

### 11.1 Tujuan

Setelah window app di-embed, Leader perlu bisa **mengetik perintah**
ke app tersebut dan **membaca response**. Ini dilakukan via Windows
UI Automation API.

### 11.2 Library: `windows` crate (Microsoft official)

```text
windows::Win32::UI::Accessibility::*
  - IUIAutomation
  - IUIAutomationElement
  - IUIAutomationCondition
  - UIA_EditControlTypeId
  - UIA_TextControlTypeId
  - UIA_ButtonControlTypeId
```

### 11.3 Alur: Mengirim Perintah ke Cursor (contoh)

```text
1. Dapatkan root element dari Cursor window (hwnd)
2. Cari element dengan:
   - ControlType = Edit
   - AutomationId = "chat-input" (atau Name yang sesuai)
3. SetValue(element, "Buat file auth.ts ...")
4. Cari tombol Send:
   - ControlType = Button
   - Name = "Send" / "Submit"
5. Invoke(send_button)
6. Tunggu response:
   - Poll element teks di area chat
   - Baca content terbaru
```

### 11.4 Fallback: Keyboard Simulation

Jika UI Automation element tidak ditemukan (app menggunakan custom
rendering):

```text
1. SetForegroundWindow(cursor_hwnd)
2. SendInput: Ctrl+L (focus chat) atau klik koordinat tertentu
3. SendInput: ketik perintah karakter per karakter
4. SendInput: Enter
5. Tunggu, lalu capture screenshot + OCR untuk membaca response
```

Metode ini lebih fragile tapi universal — bisa bekerja dengan app
apa pun.

### 11.5 Screenshot + AI Vision (Fallback Terakhir)

Jika keyboard simulation juga tidak cukup:

1. Capture screenshot panel.
2. Kirim ke vision-capable LLM (GPT-4o / Gemini / Claude).
3. LLM menginterpretasi apa yang ada di layar.
4. LLM menghasilkan instruksi keyboard/mouse selanjutnya.

Ini adalah teknik "Computer Use" yang sudah didemonstrasikan oleh
Anthropic. Sebagai last-resort, bukan metode utama.

---

## 12. Antarmuka Pengguna (UI/UX)

### 12.1 Layout 3 Kolom (Progress + Workers + Leader)

```text
+--------------------------------------------------------------------------+
|                          Synergy Workspace                               |
+--------+----------------------------------------+-----------------------+
|PROGRESS|            WORKERS (tengah)             |    LEADER (kanan)     |
|  (kiri)|                                        |                       |
| ┌────┐ | ┌─────────────┐  ┌─────────────┐       | ┌───────────────────┐ |
| │ ✓ 1│ | │  Worker 1   │  │  Worker 2   │       | │                   │ |
| │ ✓ 2│ | │  opencode   │  │  opencode   │       | │   Kiro / Cursor / │ |
| │ ▶ 3│ | │  IP-A 🟢    │  │  IP-B 🟡    │       | │   Terminal / API  │ |
| │ ○ 4│ | │  > writing  │  │  > thinking │       | │                   │ |
| │ ○ 5│ | │  > ...      │  │  > ...      │       | │   (embedded app   │ |
| │ ○ 6│ | ├─────────────┤  ├─────────────┤       | │    atau chat UI)  │ |
| │ ○ 7│ | │  Worker 3   │  │  Worker 4   │       | │                   │ |
| │   │ | │  opencode   │  │  opencode   │       | │   Leader melihat  │ |
| │    │ | │  IP-C 🟢    │  │  IP-D ⚪    │       | │   semua output    │ |
| │    │ | │  ✓ done     │  │  idle       │       | │   Worker dan      │ |
| │    │ | ├─────────────┤  ├─────────────┤       | │   mengirim task   │ |
| │    │ | │  Worker 5   │  │  Worker 6   │       | │   berikutnya.     │ |
| │    │ | │  opencode   │  │  opencode   │       | │                   │ |
| │    │ | │  IP-E ⚪    │  │  IP-F ⚪    │       | │                   │ |
| │    │ | │  idle       │  │  idle       │       | │                   │ |
| └────┘ | └─────────────┘  └─────────────┘       | └───────────────────┘ |
|  ~10%  |            ~55%                        |        ~35%           |
+--------+----------------------------------------+-----------------------+
|  [Leader: Kiro ●]  [Workers: 2/6 busy]  [Proxies: 6/6 ✓]  [Tasks: 3/7] |
+--------------------------------------------------------------------------+
```

Rasio kolom: **Progress 10% — Workers 55% — Leader 35%**
(bisa di-drag untuk resize).

### 12.2 Panel Kiri — Progress / Walkthrough

Panel sempit yang menampilkan **rangkuman progres** secara real-time:

```text
┌─ Progress ──────────┐
│                      │
│  Fitur: Login Auth   │
│  ────────────────    │
│  ✓ 1. Migration DB   │
│  ✓ 2. Model User     │
│  ▶ 3. Controller     │  ← sedang dikerjakan (Worker 1)
│    ○ 4. Login Page    │
│    ○ 5. Unit Test     │
│    ○ 6. Integration   │
│    ○ 7. Review        │
│                      │
│  ════════════════    │
│  Progress: 2/7 (28%) │
│  ▓▓▓░░░░░░░░ 28%    │
│                      │
│  ── Worker Status ── │
│  W1: ▶ Task 3        │
│  W2: ▶ Task 4        │
│  W3: ✓ idle          │
│  W4: ✓ idle          │
│  W5: ✓ idle          │
│  W6: ✓ idle          │
│                      │
│  ── Recent Log ────  │
│  14:02 W1 mulai t3   │
│  14:01 W3 selesai t2  │
│  14:00 W2 mulai t4   │
│  13:58 W1 selesai t1  │
└──────────────────────┘
```

Fitur panel Progress:
- **Task checklist**: daftar task dengan status (✓ done / ▶ running / ○ pending).
- **Progress bar**: persentase keseluruhan.
- **Worker status**: ringkasan singkat apa yang dikerjakan tiap Worker.
- **Activity log**: timeline event terbaru.
- **Collapsible**: klik untuk expand/collapse detail.

### 12.3 Panel Tengah — Workers Grid

Area terbesar. Menampilkan **terminal OpenCode hidup** untuk setiap
Worker. User bisa **melihat langsung** apa yang sedang dikerjakan
setiap Worker — bahkan bisa **klik panel Worker** untuk interaksi
manual (misal ketik `/model` untuk ganti model di OpenCode).

```text
┌─ Worker 1 ─────────────────────────────────────┐
│ ┌─ Header ───────────────────────────────────┐  │
│ │ 🟢 Worker 1  │  Proxy: IP-A  │  Task: #3   │  │
│ │ Model: claude-3.5-sonnet  │  ▶ Working     │  │
│ └────────────────────────────────────────────┘  │
│                                                 │
│  $ opencode                                     │
│  Using model: claude-3.5-sonnet (free)          │
│                                                 │
│  > Buat AuthController dengan method            │
│    login, register, dan logout...               │
│                                                 │
│  ⠋ Generating src/controllers/AuthController.ts │
│  ████████░░░░ 67%                               │
│                                                 │
│  [Klik panel ini untuk interaksi manual]        │
└─────────────────────────────────────────────────┘
```

Fitur panel Worker:
- **Header**: nama Worker, proxy IP, task yang dikerjakan, status.
- **Terminal live**: output OpenCode real-time (scrollable).
- **Model indicator**: model apa yang dipakai OpenCode saat ini.
- **Interactive**: klik panel → bisa ketik langsung ke terminal
  (misal `/model gpt-4o-mini` untuk ganti model, atau perintah
  manual lainnya).
- **Status badge**: 🟢 working / 🟡 thinking / ⚪ idle / 🔴 error.
- **Grid layout**: 3×2 grid (6 Worker). Jika Worker kurang dari 6,
  panel kosong ditampilkan sebagai placeholder dengan tombol
  "+ Tambah Worker".

### 12.4 Panel Kanan — Leader

Panel besar untuk **AI utama** yang user pilih. Tampilan tergantung
jenis app:

**A) CLI mode** (OpenCode / Aider / Claude CLI):
```text
┌─ Leader (OpenCode) ──────────────────┐
│                                      │
│  $ opencode                          │
│  > /model claude-opus-4              │
│  Model set to: claude-opus-4         │
│                                      │
│  > Buat fitur login lengkap untuk    │
│    aplikasi Express.js ini. Pecah    │
│    menjadi sub-task dan delegasikan  │
│    ke Workers.                       │
│                                      │
│  Baik, saya akan memecah menjadi:    │
│  1. Migration tabel users            │
│  2. Model User + validasi            │
│  3. AuthController (login/register)  │
│  4. Halaman login (EJS/Blade)        │
│  5. Unit test auth                   │
│  6. Integration test                 │
│  7. Review & cleanup                 │
│                                      │
│  Mendelegasikan task 1-2 ke Worker   │
│  1 dan 2...                          │
│                                      │
└──────────────────────────────────────┘
```

**B) GUI mode** (Kiro / Cursor):
```text
┌─ Leader (Kiro) ──────────────────────┐
│                                      │
│  ┌──────────────────────────────┐    │
│  │                              │    │
│  │   [Kiro IDE embedded di      │    │
│  │    sini — window asli Kiro   │    │
│  │    yang di-SetParent ke      │    │
│  │    panel ini]                │    │
│  │                              │    │
│  │   User bisa pakai Kiro      │    │
│  │   seperti biasa + Synergy   │    │
│  │   membaca output untuk      │    │
│  │   koordinasi Worker         │    │
│  │                              │    │
│  └──────────────────────────────┘    │
│                                      │
└──────────────────────────────────────┘
```

**C) API mode** (langsung pakai API key):
```text
┌─ Leader (API: claude-opus-4) ────────┐
│                                      │
│  ┌─ Chat UI ──────────────────────┐  │
│  │ 👤 Buat fitur login lengkap    │  │
│  │                                │  │
│  │ 🤖 Saya akan memecah menjadi  │  │
│  │    7 sub-task:                 │  │
│  │    1. Migration tabel users    │  │
│  │    2. Model User ...           │  │
│  │    [Approve & Execute]         │  │
│  │    [Revise]                    │  │
│  └────────────────────────────────┘  │
│                                      │
│  ┌─ Model ────────────────────────┐  │
│  │ Provider: Anthropic            │  │
│  │ Model:    claude-opus-4        │  │
│  │ [Change Model ▼]              │  │
│  └────────────────────────────────┘  │
└──────────────────────────────────────┘
```

### 12.5 Layout Modes

User bisa switch layout:

| Mode | Shortcut | Deskripsi |
| --- | --- | --- |
| **Default (3-col)** | `Ctrl+Shift+1` | Progress + Workers + Leader (layout utama) |
| **Focus Worker** | `Ctrl+Shift+2` | 1 Worker full-screen (double-klik panel Worker) |
| **Focus Leader** | `Ctrl+Shift+3` | Leader full-screen + Progress sidebar |
| **Minimal** | `Ctrl+Shift+4` | Leader full + Workers sebagai tab kecil di bawah |
| **Wide Workers** | `Ctrl+Shift+5` | Progress + Workers lebar (Leader disembunyikan) |

### 12.6 Worker Model Setup

User bisa mengatur model OpenCode di setiap Worker **langsung dari
panel terminal**:

```text
Klik panel Worker 1 → fokus terminal → ketik:

  /model claude-3.5-sonnet       ← pakai model ini
  /model gpt-4o-mini             ← atau ini
  /model deepseek-coder          ← atau model gratis lainnya

OpenCode akan menggunakan model yang dipilih untuk task berikutnya.
```

Atau konfigurasi sekaligus semua Worker via Settings:

```toml
[workers.model]
default = "claude-3.5-sonnet"      # model default semua Worker
worker_1 = "claude-3.5-sonnet"     # override per Worker (opsional)
worker_2 = "gpt-4o-mini"
worker_3 = "deepseek-coder"
```

### 12.7 Status Bar (Global, Bawah)

```text
┌──────────────────────────────────────────────────────────────────────┐
│ [Leader: Kiro ●]  [Workers: 2/6 busy]  [Proxies: 6/6 ✓]            │
│ [Tasks: 3/7 done ▓▓▓▓░░░░░░ 43%]  [Uptime: 01:23:45]              │
└──────────────────────────────────────────────────────────────────────┘
```

### 12.8 Tema & Aksesibilitas

- Tema gelap default (cocok untuk developer).
- Font: system monospace (Cascadia Code di Windows).
- Keyboard shortcut:
  - `Ctrl+0`: focus Progress panel.
  - `Ctrl+1..6`: focus Worker 1-6.
  - `Ctrl+7`: focus Leader panel.
  - `Ctrl+Tab`: cycle antar panel.
  - `Ctrl+Shift+1..5`: switch layout mode.
  - `Ctrl+K`: command palette.
  - `Escape`: kembali dari focus mode ke layout default.

---

## 13. App Adapter System

### 13.1 Konsep

Setiap app yang didukung Synergy punya **adapter** — modul yang tahu:

1. Cara **meluncurkan** app.
2. Cara **embed** (PTY spawn atau SetParent).
3. Cara **mengirim perintah** (stdin write atau UI Automation).
4. Cara **membaca output** (stdout parse atau UI element read).
5. Cara **mendeteksi status** (idle/working/error/done).

### 13.2 Trait Adapter (Rust, konseptual)

```rust
#[async_trait]
trait AppAdapter: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn app_type(&self) -> AppType; // CLI | GUI

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle>;
    async fn embed(&self, handle: &AppHandle, panel: PanelId) -> Result<()>;
    async fn send_command(&self, handle: &AppHandle, text: &str) -> Result<()>;
    async fn read_output(&self, handle: &AppHandle) -> Result<OutputStream>;
    async fn detect_status(&self, handle: &AppHandle) -> Result<AppStatus>;
    async fn detach(&self, handle: &AppHandle) -> Result<()>;
}

enum AppType { CLI, GUI }
enum AppStatus { Idle, Working, Error(String), Done }
```

### 13.3 Adapter Registry

```toml
# adapters.toml — daftar adapter bawaan

[[adapter]]
id   = "opencode"
type = "cli"
bin  = "opencode"
desc = "OpenCode CLI — free AI coding assistant"

[[adapter]]
id   = "aider"
type = "cli"
bin  = "aider"
desc = "Aider — AI pair programming in terminal"

[[adapter]]
id   = "cursor"
type = "gui"
bin  = "Cursor.exe"
window_title = "Cursor"
desc = "Cursor — AI-first code editor"

[[adapter]]
id   = "kiro"
type = "gui"
bin  = "Kiro.exe"
window_title = "Kiro"
desc = "Kiro — AI IDE by Amazon"
```

### 13.4 Custom Adapter

User bisa menambahkan adapter untuk app yang belum didukung secara
resmi, via konfigurasi:

```toml
[[adapter.custom]]
id           = "my-cli-tool"
type         = "cli"
bin          = "my-tool.exe"
idle_pattern = "^> $"         # regex: kapan tool idle
done_pattern = "Task complete"
error_pattern = "^Error:"
```

---

## 14. Supported Apps (Leader)

### 14.1 Phase 1 — CLI (v0.1)

| App | Status | Catatan |
| --- | --- | --- |
| **OpenCode** | Primary | Default untuk Worker. Bisa juga jadi Leader. |
| **Aider** | Supported | CLI AI pair programmer. |
| **Claude CLI** | Supported | Official Anthropic CLI. |
| **Codex CLI** | Supported | OpenAI Codex terminal. |
| **Ollama** | Supported | Lokal, gratis. |
| **Terminal (bash/pwsh)** | Supported | Untuk task non-AI (git, npm, dll). |

### 14.2 Phase 2 — GUI (v0.2)

| App | Status | Catatan |
| --- | --- | --- |
| **Cursor** | Planned | Electron-based, UI Automation friendly. |
| **Kiro** | Planned | Electron-based. |
| **VS Code + Copilot** | Planned | Electron-based. |
| **Windsurf** | Planned | Electron-based. |

### 14.3 Phase 3 — Web/Advanced (v0.3+)

| App | Status | Catatan |
| --- | --- | --- |
| **ChatGPT Web** | Exploratory | Via Playwright browser automation. |
| **Claude.ai Web** | Exploratory | Via Playwright. |
| **Google AI Studio** | Exploratory | Via Playwright. |

---

## 15. Protokol Komunikasi Leader–Worker

### 15.1 Alur Komunikasi

```text
User ──(input)──→ Leader ──(task)──→ Orchestrator
                                        │
                    ┌───────────────────┼───────────────────┐
                    │                   │                   │
               ┌────▼───┐         ┌────▼───┐         ┌────▼───┐
               │Worker 1│         │Worker 2│         │Worker 3│
               └────┬───┘         └────┬───┘         └────┬───┘
                    │                   │                   │
                    └───────────────────┼───────────────────┘
                                        │
                                   ┌────▼──────┐
                                   │Orchestrator│
                                   │(aggregate) │
                                   └────┬──────┘
                                        │
                                   ┌────▼──┐
                                   │Leader │ (review + next steps)
                                   └───────┘
```

### 15.2 Message Types (Internal)

```json
{
  "id": "msg_01H...",
  "ts": "2026-05-20T14:00:00Z",
  "type": "TaskAssign",
  "from": "leader",
  "to": "worker:3",
  "payload": {
    "task_id": "t1",
    "instruction": "Buat file src/auth/login.ts ...",
    "files_target": ["src/auth/login.ts"],
    "depends_on": []
  }
}
```

| Type | From → To | Deskripsi |
| --- | --- | --- |
| `TaskAssign` | Orchestrator → Worker | Kirim task ke Worker (tulis ke stdin PTY) |
| `TaskProgress` | Worker → Orchestrator | Output streaming dari Worker (baca stdout) |
| `TaskDone` | Worker → Orchestrator | Worker selesai (detected via output pattern) |
| `TaskFailed` | Worker → Orchestrator | Worker gagal (error pattern / timeout) |
| `TaskReassign` | Orchestrator → Worker | Task dipindah ke Worker lain |
| `LeaderQuery` | Orchestrator → Leader | Tanya Leader untuk keputusan/instruksi |
| `LeaderResponse` | Leader → Orchestrator | Jawaban Leader (task list / koreksi) |
| `StatusUpdate` | Orchestrator → UI | Update status untuk tampilan |

### 15.3 Alur Detail: Kirim Task ke Worker

```text
1. Orchestrator memilih Worker idle (misal Worker 2)
2. Tandai Worker 2 sebagai "working"
3. Set file lock: files_target = ["src/auth/login.ts"]
4. Tulis ke PTY stdin Worker 2:
   → instruction text
5. Mulai monitoring stdout Worker 2:
   → stream ke UI panel Worker 2
   → parsing untuk status detection
6. Saat idle_pattern terdeteksi:
   → tandai Worker 2 sebagai "done" atau "idle"
   → release file lock
   → notify Orchestrator → notify Leader
```

### 15.4 ALUR UTAMA: Synergy Sebagai Broker (End-to-End)

Ini adalah mekanisme inti Synergy. Leader dan Worker **tidak bicara
langsung** — Synergy Orchestrator yang membaca, meneruskan, dan
melaporkan antara keduanya.

```text
User                Synergy               Leader              Workers
  │                (Orchestrator)            │                   │
  │                                          │                   │
  │ STEP 1: User kirim perintah              │                   │
  │── "buat login" ──→│                      │                   │
  │                    │                      │                   │
  │                    │ STEP 2: Teruskan ke Leader              │
  │                    │── tulis ke stdin ───→│                   │
  │                    │                      │                   │
  │                    │ STEP 3: Leader merespons rencana         │
  │                    │                      │── "saya pecah     │
  │                    │                      │   jadi 4 task:    │
  │                    │                      │   1. migration    │
  │                    │                      │   2. model user   │
  │                    │                      │   3. controller   │
  │                    │                      │   4. login page"  │
  │                    │←── baca stdout ──────│                   │
  │                    │                      │                   │
  │                    │ STEP 4: Parse task dari output Leader    │
  │                    │ (extract 4 task via pattern matching)    │
  │                    │                      │                   │
  │                    │ STEP 5: Distribusi ke Workers            │
  │                    │── task 1 ────────────│───────────────→ W1│
  │                    │── task 2 ────────────│───────────────→ W2│
  │                    │   (task 3,4 antri — tunggu dependency)  │
  │                    │                      │                   │
  │                    │ STEP 6: Monitor stdout tiap Worker       │
  │                    │                      │              W1: selesai
  │                    │←──── baca stdout W1 ─│───────────────── │
  │                    │                      │              W2: selesai
  │                    │←──── baca stdout W2 ─│───────────────── │
  │                    │                      │                   │
  │                    │ STEP 7: Compose laporan → kirim ke Leader│
  │                    │── "[LAPORAN W1] ──────→│                 │
  │                    │    task 1 selesai.     │                 │
  │                    │    [LAPORAN W2]        │                 │
  │                    │    task 2 selesai." ──→│                 │
  │                    │                      │                   │
  │                    │ STEP 8: Leader review                   │
  │                    │                      │── "review:        │
  │                    │                      │   task 1 oke ✓    │
  │                    │                      │   task 2 perlu    │
  │                    │                      │   perbaikan..."   │
  │                    │←── baca stdout ──────│                   │
  │                    │                      │                   │
  │                    │ STEP 9: Parse koreksi → kirim ke Worker  │
  │                    │── koreksi ───────────│───────────────→ W2│
  │                    │                      │              W2: fixed
  │                    │←──── baca stdout W2 ─│───────────────── │
  │                    │                      │                   │
  │                    │ STEP 10: Lapor fix ke Leader             │
  │                    │── "W2 sudah fix" ───→│                   │
  │                    │                      │── "semua oke ✓"   │
  │                    │←── baca stdout ──────│                   │
  │                    │                      │                   │
  │                    │ STEP 11: Lapor ke User                  │
  │←── "Selesai! ✓" ──│                      │                   │
```

### 15.5 STEP-BY-STEP: Parsing Task dari Output Leader

Ketika Leader merespons rencana, Synergy harus mengekstrak task dari
output teks biasa. Berikut mekanismenya:

**Input** (teks stdout Leader):
```text
Baik, saya akan memecah menjadi 4 task:
1. Buat migration tabel users dengan field email, password, name
2. Buat model User dengan bcrypt hashing
3. Buat AuthController dengan method login, register, logout
4. Buat halaman login menggunakan EJS template
```

**Parsing** (pseudocode):
```javascript
function parseTasksFromLeaderOutput(text) {
  const tasks = [];
  const lines = text.split('\n');
  
  for (const line of lines) {
    // Pattern 1: numbered list "1. ...", "2. ...", dst
    const match = line.match(/^\s*(\d+)\.\s+(.+)$/);
    if (match) {
      tasks.push({
        id: `t${match[1]}`,
        instruction: match[2].trim(),
        status: 'pending',
        depends_on: [],
        files_target: extractFilePaths(match[2]) // cari path file
      });
    }
    
    // Pattern 2: dash list "- ...", "* ...", dst
    const dashMatch = line.match(/^\s*[-*]\s+(.+)$/);
    if (dashMatch && looksLikeTask(dashMatch[1])) {
      tasks.push({ /* ... */ });
    }
    
    // Pattern 3: "Task N:" format
    const taskMatch = line.match(/^Task\s+(\d+)\s*:\s*(.+)$/i);
    if (taskMatch) {
      tasks.push({ /* ... */ });
    }
  }
  
  // Auto-detect dependency: jika task menyebut output task lain
  detectDependencies(tasks);
  
  return tasks;
}
```

**Output** (parsed tasks):
```json
[
  {"id":"t1", "instruction":"Buat migration tabel users...", "depends_on":[]},
  {"id":"t2", "instruction":"Buat model User dengan bcrypt...", "depends_on":[]},
  {"id":"t3", "instruction":"Buat AuthController...", "depends_on":["t1","t2"]},
  {"id":"t4", "instruction":"Buat halaman login...", "depends_on":["t3"]}
]
```

**Dependency detection**: Synergy otomatis mendeteksi dependency
berdasarkan logika:
- Jika task menyebut file yang dibuat task lain → dependen.
- Jika task secara eksplisit bilang "setelah task X" → dependen.
- Jika tidak ada indikasi → paralel (independent).
- Default fallback: jalankan berurutan jika ragu.

### 15.6 STEP-BY-STEP: Mengirim Task ke Worker via PTY

Setelah task di-parse, Synergy mengirimnya ke Worker:

```javascript
async function sendTaskToWorker(task, worker) {
  // 1. Tandai worker sebagai busy
  worker.status = 'working';
  worker.currentTask = task;
  updateUI(worker);
  
  // 2. Set file lock (prevent conflict)
  if (task.files_target.length > 0) {
    acquireFileLock(task.files_target, worker.id);
  }
  
  // 3. Tulis instruction ke PTY stdin Worker
  //    (ini secara literal "mengetik" ke terminal OpenCode)
  const prompt = task.instruction;
  worker.pty.write(prompt + '\r'); // \r = Enter
  
  // 4. Log event
  logEvent({
    type: 'TaskAssign',
    from: 'orchestrator',
    to: `worker:${worker.id}`,
    task_id: task.id,
    instruction: task.instruction
  });
  
  // 5. Mulai monitoring output Worker ini
  monitorWorkerOutput(worker, task);
}
```

### 15.7 STEP-BY-STEP: Monitoring Output Worker & Deteksi Selesai

Synergy terus membaca stdout Worker untuk mendeteksi kapan task
selesai atau gagal:

```javascript
function monitorWorkerOutput(worker, task) {
  let outputBuffer = '';
  let lastActivityTime = Date.now();
  
  worker.pty.onData((rawData) => {
    const text = stripAnsiCodes(rawData);
    outputBuffer += text;
    lastActivityTime = Date.now();
    
    // Tampilkan di UI panel Worker (real-time)
    worker.terminalUI.write(rawData);
    
    // --- DETEKSI STATUS ---
    
    // Deteksi SELESAI: Worker kembali idle (prompt muncul lagi)
    // OpenCode menampilkan "> " saat idle
    if (isIdlePrompt(text)) {
      const result = parseWorkerResult(outputBuffer);
      onWorkerDone(worker, task, result);
      outputBuffer = '';
      return;
    }
    
    // Deteksi ERROR: pattern error terdeteksi
    if (hasErrorPattern(text)) {
      task.attempt++;
      if (task.attempt <= MAX_RETRIES) {
        // Retry: kirim ulang task
        worker.pty.write(`Fix the error above and try again\r`);
      } else {
        // Escalate ke Leader
        onWorkerFailed(worker, task, text);
        outputBuffer = '';
      }
      return;
    }
    
    // Deteksi FILE CREATED/MODIFIED
    const files = detectFileChanges(text);
    if (files.length > 0) {
      task.filesCreated = [...(task.filesCreated || []), ...files];
      updateProgressUI(task);
    }
  });
  
  // Timeout guard
  setInterval(() => {
    if (Date.now() - lastActivityTime > TIMEOUT_MS) {
      onWorkerTimeout(worker, task);
    }
  }, 5000);
}

// Pattern deteksi untuk OpenCode:
function isIdlePrompt(text) {
  // OpenCode menampilkan "> " saat siap menerima input baru
  return />\s*$/.test(text.trim());
}

function hasErrorPattern(text) {
  const patterns = [
    /error:/i, /Error:/,
    /failed/i, /FAILED/,
    /exception/i, /panic:/i,
    /command not found/,
    /exit code [1-9]/
  ];
  return patterns.some(p => p.test(text));
}

function detectFileChanges(text) {
  const files = [];
  // Pattern: "Creating src/models/User.ts"
  const createMatch = text.matchAll(/(?:Creating|Created|Writing|Wrote)\s+(\S+\.\w+)/gi);
  for (const m of createMatch) files.push(m[1]);
  // Pattern: "+++ b/src/models/User.ts" (diff output)
  const diffMatch = text.matchAll(/\+\+\+ b\/(\S+)/g);
  for (const m of diffMatch) files.push(m[1]);
  return files;
}
```

### 15.8 STEP-BY-STEP: Compose Laporan & Kirim ke Leader

Setelah Worker selesai, Synergy menyusun laporan dan mengirimnya
ke Leader untuk review:

```javascript
function onWorkerDone(worker, task, result) {
  // 1. Update status
  worker.status = 'idle';
  worker.currentTask = null;
  task.status = 'done';
  
  // 2. Release file lock
  releaseFileLock(task.files_target, worker.id);
  
  // 3. Update UI (progress panel + worker panel)
  updateProgressUI(task);
  updateWorkerUI(worker);
  
  // 4. Simpan ke database
  saveTaskResult(task, result);
  
  // 5. Cek apakah semua task batch ini sudah selesai
  const pendingTasks = getPendingTasks();
  const runningTasks = getRunningTasks();
  
  if (runningTasks.length === 0 && pendingTasks.length > 0) {
    // Masih ada task pending → assign ke Worker idle
    assignNextTasks();
  }
  
  if (runningTasks.length === 0 && pendingTasks.length === 0) {
    // Semua task batch ini selesai → lapor ke Leader
    sendBatchReportToLeader();
  }
}

function sendBatchReportToLeader() {
  const completedTasks = getCompletedTasks();
  
  // Compose laporan dalam format yang Leader bisa baca
  let report = '\n';
  report += '══════════════════════════════════════\n';
  report += '  LAPORAN HASIL WORKER\n';
  report += '══════════════════════════════════════\n\n';
  
  for (const task of completedTasks) {
    const status = task.status === 'done' ? '✓ SELESAI' : '✗ GAGAL';
    report += `[Worker ${task.workerId}] Task #${task.id}: ${task.title}\n`;
    report += `  Status : ${status}\n`;
    report += `  Waktu  : ${task.duration}s\n`;
    if (task.filesCreated?.length > 0) {
      report += `  File   : ${task.filesCreated.join(', ')}\n`;
    }
    if (task.error) {
      report += `  Error  : ${task.error}\n`;
    }
    report += '\n';
  }
  
  report += '══════════════════════════════════════\n';
  report += 'Silakan review hasil di atas.\n';
  report += 'Balas "OK" jika semua benar, atau\n';
  report += 'berikan koreksi untuk task tertentu.\n';
  report += '══════════════════════════════════════\n';
  
  // Tulis laporan ke stdin Leader
  leaderPty.write(report + '\r');
  
  // Monitor response Leader (kembali ke loop §15.7)
  monitorLeaderForReview();
}

function monitorLeaderForReview() {
  leaderPty.onData((rawData) => {
    const text = stripAnsiCodes(rawData);
    
    // Leader bilang "OK" / "approved" / "semua benar"
    if (isApproval(text)) {
      onAllTasksApproved();
      return;
    }
    
    // Leader memberikan koreksi
    const corrections = parseCorrectionFromLeader(text);
    if (corrections.length > 0) {
      for (const correction of corrections) {
        // Kirim koreksi ke Worker yang relevan
        const worker = getIdleWorker();
        sendTaskToWorker({
          id: correction.taskId,
          instruction: correction.instruction,
          isCorrection: true
        }, worker);
      }
    }
  });
}

function isApproval(text) {
  const approvalPatterns = [
    /\bok\b/i, /approved/i, /\boke\b/i,
    /semua (sudah |sudah )?benar/i,
    /semua (sudah |sudah )?oke/i,
    /lgtm/i, /looks good/i,
    /✓|✅|👍/
  ];
  return approvalPatterns.some(p => p.test(text));
}

function onAllTasksApproved() {
  // Update UI: semua hijau
  updateProgressUI({ allDone: true });
  
  // Notifikasi ke user
  showNotification('Semua task selesai dan disetujui Leader ✓');
  
  // Log
  logEvent({ type: 'SessionComplete', tasks: getCompletedTasks() });
}
```

### 15.9 Contoh Alur Lengkap (Skenario Nyata)

Berikut rekaman alur dari awal sampai akhir:

```text
═══════════════════════════════════════════════════════
  CONTOH SKENARIO: "Buat fitur login"
═══════════════════════════════════════════════════════

[15:19:00] USER → LEADER:
  "Buat fitur login untuk Express.js app ini.
   Database pakai PostgreSQL."

[15:19:05] SYNERGY: Meneruskan ke Leader (tulis ke stdin PTY Leader)

[15:19:12] LEADER merespons:
  "Baik, saya pecah jadi 4 task:
   1. Buat migration tabel users (email, password, name)
   2. Buat model User dengan bcrypt hashing
   3. Buat AuthController (login/register/logout)
   4. Buat halaman login (EJS template + CSS)"

[15:19:12] SYNERGY: Parse output → 4 task terdeteksi
  → Task 1 & 2: tidak ada dependency → PARALEL
  → Task 3: butuh Task 1 & 2 selesai → BLOCKED
  → Task 4: butuh Task 3 selesai → BLOCKED

[15:19:13] SYNERGY → WORKER 1:
  PTY stdin: "Buat migration tabel users (email, password, name)
  untuk PostgreSQL. File: src/db/migrations/001_users.sql"

[15:19:13] SYNERGY → WORKER 2:
  PTY stdin: "Buat model User dengan bcrypt hashing.
  File: src/models/User.ts"

[15:19:13] Progress Panel:
  ✓ — (none)
  ▶ 1. Migration (Worker 1)
  ▶ 2. Model User (Worker 2)
  ○ 3. AuthController (blocked, waiting 1+2)
  ○ 4. Login page (blocked, waiting 3)

[15:19:45] WORKER 2 stdout: "✓ File created successfully"
  → SYNERGY: idle_pattern detected → Worker 2 DONE
  → SYNERGY: file detected: src/models/User.ts (78 lines)

[15:20:10] WORKER 1 stdout: "✓ Migration created"
  → SYNERGY: Worker 1 DONE
  → SYNERGY: file detected: src/db/migrations/001_users.sql

[15:20:10] SYNERGY: Task 1 & 2 selesai → Task 3 UNBLOCKED
  → Assign Task 3 ke Worker 1 (idle)

[15:20:11] SYNERGY → WORKER 1:
  PTY stdin: "Buat AuthController dengan method login, register,
  logout. Import User model dari src/models/User.ts.
  File: src/controllers/AuthController.ts"

[15:20:55] WORKER 1: Task 3 DONE
  → SYNERGY: Task 3 selesai → Task 4 UNBLOCKED
  → Assign Task 4 ke Worker 2 (idle)

[15:21:30] WORKER 2: Task 4 DONE

[15:21:30] SYNERGY: Semua 4 task selesai. Compose laporan:

[15:21:31] SYNERGY → LEADER:
  PTY stdin:
  "══════════════════════════════════════
     LAPORAN HASIL WORKER
   ══════════════════════════════════════

   [Worker 1] Task #1: Migration tabel users
     Status : ✓ SELESAI
     File   : src/db/migrations/001_users.sql (18 lines)
     Waktu  : 57s

   [Worker 2] Task #2: Model User + bcrypt
     Status : ✓ SELESAI
     File   : src/models/User.ts (78 lines)
     Waktu  : 32s

   [Worker 1] Task #3: AuthController
     Status : ✓ SELESAI
     File   : src/controllers/AuthController.ts (95 lines)
     Waktu  : 45s

   [Worker 2] Task #4: Halaman login
     Status : ✓ SELESAI
     File   : src/views/login.ejs (52 lines),
              src/public/css/auth.css (38 lines)
     Waktu  : 35s

   ══════════════════════════════════════
   Silakan review. Balas OK atau koreksi.
   ══════════════════════════════════════"

[15:21:45] LEADER merespons:
  "Saya review:
   ✅ Task 1: migration OK, field lengkap
   ✅ Task 2: model OK, bcrypt.hash(password, 12) benar
   ⚠️ Task 3: AuthController kurang try-catch di method login,
      dan belum return error 401 jika password salah
   ✅ Task 4: halaman login OK

   Koreksi Task 3: Tambahkan try-catch di method login dan
   return res.status(401).json({error: 'Invalid credentials'})
   jika password tidak match."

[15:21:45] SYNERGY: Parse response → 1 koreksi detected (Task 3)

[15:21:46] SYNERGY → WORKER 1:
  PTY stdin: "Perbaiki AuthController: tambahkan try-catch di
  method login dan return res.status(401).json({error: 'Invalid
  credentials'}) jika password tidak match."

[15:22:10] WORKER 1: Koreksi DONE

[15:22:11] SYNERGY → LEADER:
  PTY stdin:
  "[LAPORAN KOREKSI]
   Worker 1 sudah memperbaiki Task #3.
   Perubahan: tambah try-catch + 401 response.
   Silakan review ulang."

[15:22:18] LEADER: "Oke, semua sudah benar ✓"

[15:22:18] SYNERGY: Approval detected!
  → Update Progress Panel: 4/4 done (100%) ✅
  → Notifikasi ke User: "Fitur login selesai! Approved oleh Leader."
  → Log: SessionComplete

═══════════════════════════════════════════════════════
  TOTAL WAKTU: 3 menit 18 detik
  TOTAL FILE: 5 file dibuat/diubah
═══════════════════════════════════════════════════════
```

## 16. Task Management & Orchestration

### 16.1 Task Lifecycle

```text
  ┌────────┐  assign   ┌─────────┐  output    ┌──────┐
  │PENDING │ ────────→ │ RUNNING │ ────────→  │ DONE │
  └────────┘           └─────────┘            └──────┘
       │                    │
       │  dependencies      │  error / timeout
       │  not met            │
       ▼                    ▼
  ┌────────┐           ┌────────┐  retry    ┌─────────┐
  │BLOCKED │           │FAILED  │ ────────→ │ RUNNING │
  └────────┘           └────────┘           └─────────┘
                            │
                            │  max retries exceeded
                            ▼
                       ┌──────────┐
                       │ESCALATED │ → tanya Leader
                       └──────────┘
```

### 16.2 Retry & Escalation

```toml
[task]
max_retries        = 2
timeout_minutes    = 10
escalate_to_leader = true
```

Jika Worker gagal 2 kali:
1. Task di-escalate ke Leader.
2. Leader menganalisis error.
3. Leader bisa: revise instruksi, split task, atau skip.

### 16.3 Task Queue

```text
Priority Queue (FIFO within same priority):
  ┌───────────────────────────────────────┐
  │ [P0] Critical: fix build error        │ ← dari Leader escalation
  │ [P1] Normal: buat auth controller     │
  │ [P1] Normal: buat login page          │
  │ [P2] Low: tulis unit test             │
  └───────────────────────────────────────┘
```

---

## 17. Skema Data & Penyimpanan

### 17.1 Database: SQLite (`~/.synergy/state.db`)

```sql
CREATE TABLE project (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  root_path   TEXT NOT NULL,
  created_at  TEXT NOT NULL
);

CREATE TABLE session (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL REFERENCES project(id),
  started_at  TEXT NOT NULL,
  ended_at    TEXT,
  leader_app  TEXT NOT NULL,
  worker_count INTEGER NOT NULL DEFAULT 6
);

CREATE TABLE task (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES session(id),
  title       TEXT NOT NULL,
  instruction TEXT NOT NULL,
  status      TEXT NOT NULL CHECK (status IN
    ('pending','blocked','running','done','failed','escalated')),
  worker_id   INTEGER,
  depends_on  TEXT,
  files_target TEXT,
  attempt     INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL,
  started_at  TEXT,
  ended_at    TEXT
);

CREATE TABLE worker (
  id          INTEGER PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES session(id),
  proxy_addr  TEXT,
  proxy_label TEXT,
  status      TEXT NOT NULL CHECK (status IN ('idle','working','error','offline')),
  pid         INTEGER
);

CREATE TABLE event_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id  TEXT NOT NULL,
  ts          TEXT NOT NULL,
  source      TEXT NOT NULL,
  type        TEXT NOT NULL,
  payload     TEXT NOT NULL
);

CREATE TABLE proxy (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  address     TEXT NOT NULL,
  label       TEXT,
  healthy     INTEGER NOT NULL DEFAULT 1,
  last_check  TEXT
);
```

### 17.2 Konfigurasi: `%APPDATA%\Synergy\config.toml`

```toml
[general]
language = "id"
theme    = "dark"
project_dir = "C:\\Users\\user\\Projects\\myapp"

[leader]
adapter = "kiro"           # "opencode" | "kiro" | "cursor" | "api" | ...

[leader.api]               # hanya jika adapter = "api"
provider = "anthropic"
model    = "claude-opus-4"

[workers]
count   = 6
adapter = "opencode"       # default semua worker pakai OpenCode

[proxy]
mode = "list"
# ... daftar proxy (lihat §7.2)
```

---

## 18. Tech Stack & Dependensi Rust

### 18.1 Crate Utama

| Area | Crate | Catatan |
| --- | --- | --- |
| UI Framework | `tauri` v2 | Multi-window, webview-based. Atau `gpui` untuk native rendering. |
| Async runtime | `tokio` `[full]` | |
| PTY | `portable-pty` | ConPTY di Windows. Spawn + read/write CLI app. |
| Win32 API | `windows` (Microsoft official) | SetParent, FindWindow, UI Automation, SendInput. |
| Terminal render | `alacritty_terminal` | Parse VT sequences untuk render output di panel. |
| HTTP client | `reqwest` + `rustls` | Untuk API mode Leader + proxy config per-request. |
| Database | `rusqlite` + bundled sqlite | State, tasks, events. |
| Serialization | `serde`, `serde_json`, `toml` | |
| IDs | `ulid` | |
| Process management | `tokio::process`, `sysinfo` | Spawn, monitor, kill processes. |
| Proxy support | `reqwest` (built-in proxy), env vars | `HTTP_PROXY` / `HTTPS_PROXY` per process. |
| Error handling | `thiserror`, `anyhow` | |
| Telemetry | `tracing`, `tracing-subscriber` | |
| Testing | `insta`, `assert_cmd` | |

### 18.2 Dependensi Khusus Windows

| Area | Crate / Tool | Catatan |
| --- | --- | --- |
| Win32 bindings | `windows` | SetParent, UI Automation, SendInput. |
| Manifest | `embed-manifest` | dpiAware, longPathAware. |
| Resource embed | `winres` | Icon, version info. |
| Single-instance | `single-instance` | Cegah multi-launch. |
| Installer | `cargo-wix` (MSI) | |

---

## 19. Struktur Repository

```text
synergy/
|-- Cargo.toml                 # workspace
|-- rust-toolchain.toml
|-- README.md
|-- LICENSE-MIT
|-- LICENSE-APACHE
|-- docs/
|   `-- SYNERGY.md             # dokumen ini
|-- crates/
|   |-- synergy-app/           # entry-point biner desktop
|   |-- synergy-ui/            # UI rendering (Tauri webview / GPUI)
|   |-- synergy-core/          # orchestrator engine, task manager
|   |-- synergy-adapter/       # trait AppAdapter + implementasi per-app
|   |   `-- src/
|   |       |-- adapter.rs     # trait AppAdapter
|   |       |-- cli_opencode.rs
|   |       |-- cli_aider.rs
|   |       |-- cli_generic.rs
|   |       |-- gui_cursor.rs
|   |       |-- gui_kiro.rs
|   |       `-- api_direct.rs  # untuk Leader API mode
|   |-- synergy-pty/           # PTY multiplexer (portable-pty wrapper)
|   |-- synergy-win32/         # Win32 embedding + UI Automation
|   |-- synergy-proxy/         # proxy manager, health check, assignment
|   |-- synergy-proto/         # shared types: messages, task, event
|   |-- synergy-config/        # config loader (toml, env)
|   |-- synergy-db/            # SQLite state & event log
|   `-- synergy-tests/         # integration & e2e tests
|-- adapters.toml              # adapter registry
|-- installer/
|   `-- windows/
|       |-- wix/
|       |   `-- main.wxs
|       `-- resources/
|           |-- synergy.ico
|           `-- synergy.manifest
|-- scripts/
|   |-- dev.ps1
|   `-- dev.sh
`-- .github/
    `-- workflows/
        |-- ci.yml
        `-- release-windows.yml
```

Aturan dependensi (acyclic):

```text
app  -> ui -> core -> { adapter, pty, win32, proxy, db } -> proto
adapter -> { pty, win32 }
all -> { config }
```

---

## 20. Build, Install, Run

### 20.1 Prasyarat Windows

- **Windows 10 21H2 / Windows 11** (x86_64).
- **Rust** stable (`x86_64-pc-windows-msvc`).
- **Visual Studio 2022 Build Tools** (workload "Desktop development
  with C++").
- **Git for Windows**.
- **OpenCode CLI** terinstal (`npm i -g @anthropic-ai/opencode`
  atau binary download).
- **(Opsional)** Cursor / Kiro / app GUI lain yang ingin dipakai
  sebagai Leader.
- **(Opsional)** Daftar proxy SOCKS5/HTTP untuk IP isolation Worker.

### 20.2 Build Dev

```powershell
git clone https://github.com/<owner>/synergy.git
cd synergy
cargo build --workspace
.\scripts\dev.ps1
```

### 20.3 Build Release

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo wix --no-build --nocapture
```

### 20.4 Lokasi Data Runtime (Windows)

| Path | Isi |
| --- | --- |
| `%APPDATA%\Synergy\config.toml` | Konfigurasi user |
| `%APPDATA%\Synergy\state.db` | SQLite state |
| `%APPDATA%\Synergy\adapters.toml` | Custom adapter definitions |
| `%LOCALAPPDATA%\Synergy\logs\` | Log harian |
| `%ProgramFiles%\Synergy\` | Install directory |

---

## 21. Roadmap & Milestone

### Phase 1 — CLI Foundation (Minggu 1–6)

**Target: Leader CLI + 6 Worker OpenCode + Proxy Manager**

- [ ] Workspace Cargo + CI dasar.
- [ ] Crate `synergy-proto`: tipe shared (Task, Message, Event).
- [ ] Crate `synergy-pty`: PTY multiplexer via `portable-pty`.
  - Spawn N proses CLI secara independen.
  - Read/write stdin/stdout per PTY.
- [ ] Crate `synergy-proxy`: proxy manager.
  - Load proxy list dari config.
  - Assign proxy ke Worker via env var.
  - Health check periodik.
- [ ] Crate `synergy-adapter`: trait `AppAdapter`.
  - Impl `cli_opencode`: launch, send, read, detect status.
  - Impl `cli_generic`: untuk terminal biasa.
- [ ] Crate `synergy-core`: orchestrator engine.
  - Task queue & assignment.
  - Worker pool management.
  - File lock sederhana (prevent conflict).
  - Retry & escalation logic.
- [ ] Crate `synergy-db`: SQLite state.
- [ ] Crate `synergy-ui`: UI dasar (Tauri).
  - Layout 1+6 panel (Leader + Worker).
  - Terminal renderer per panel.
  - Status bar.
- [ ] Crate `synergy-app`: entry-point, mount semuanya.
- [ ] e2e test: Leader (CLI) mengirim 3 task ke 3 Worker → semua selesai.

**Deliverable: Synergy v0.1 — CLI-only, functional.**

---

### Phase 2 — GUI Embedding (Minggu 7–14)

**Target: Embed Cursor/Kiro di Leader Panel**

- [ ] Crate `synergy-win32`: Win32 window embedding.
  - `FindWindow` + `SetParent` + resize handling.
  - `detach` saat Synergy tutup.
- [ ] UI Automation integration.
  - Temukan chat input di Cursor/Kiro.
  - Kirim perintah via `SetValue` / keyboard simulation.
  - Baca response via element text / screenshot.
- [ ] Adapter `gui_cursor`: launch, embed, send, read.
- [ ] Adapter `gui_kiro`: launch, embed, send, read.
- [ ] Layout modes (focus, horizontal, minimal).
- [ ] Keyboard shortcut system.
- [ ] Settings UI: pilih Leader app, konfigurasi proxy.

**Deliverable: Synergy v0.2 — GUI Leader + CLI Workers.**

---

### Phase 3 — Polish & Advanced (Minggu 15+)

- [ ] Task visualization (progress bar, dependency graph).
- [ ] Leader API mode (chat UI bawaan + HTTP client ke LLM API).
- [ ] Adapter tambahan: Aider, Claude CLI, Codex CLI, Windsurf.
- [ ] Git integration: auto-commit per task, branch management.
- [ ] Session save/restore.
- [ ] Installer MSI signed (cargo-wix + signtool).
- [ ] Auto-update channel.
- [ ] Dokumentasi user.
- [ ] Open-source launch.

**Deliverable: Synergy v1.0 — Production-ready.**

---

### Estimasi Total

| Phase | Durasi | Solo Dev | Tim 2-3 |
| --- | --- | --- | --- |
| Phase 1 (CLI) | 6 minggu | 6 minggu | 3–4 minggu |
| Phase 2 (GUI) | 8 minggu | 8 minggu | 4–5 minggu |
| Phase 3 (Polish) | 6+ minggu | 6+ minggu | 4+ minggu |
| **Total** | **~20 minggu** | **~5 bulan** | **~3 bulan** |

---

## 22. Risiko & Mitigasi

| Risiko | Dampak | Mitigasi |
| --- | --- | --- |
| OpenCode mengubah output format | Worker output parsing gagal | Adapter pattern regex di file config → update tanpa rebuild. |
| OpenCode menambah rate limit lebih ketat (bukan hanya IP) | Worker terbatas | Fallback ke adapter CLI lain (Aider, Claude CLI). User bisa ganti `worker_app`. |
| Proxy tidak stabil / lambat | Worker timeout | Health check + auto-reassign ke proxy lain. Mode tanpa proxy sebagai fallback. |
| GUI app (Cursor/Kiro) update mengubah UI structure | UI Automation break | Adapter versi-detection; fallback ke keyboard simulation; fallback ke screenshot+OCR. |
| SetParent tidak bekerja pada app tertentu | Embed gagal | Fallback: buka app di window terpisah + kontrol via UI Automation tanpa embed. |
| Semua Worker edit file yang sama | Git conflict | File locking per-task. Jika conflict → eskalasi ke Leader. |
| User tidak punya proxy | Rate limit shared | Warning di UI + tetap berfungsi. Saran: gunakan free proxy atau kurangi jumlah Worker. |
| Free tier provider berubah kebijakan | Model gratis hilang | OpenCode mendukung banyak provider — bisa switch. Adapter agnostik terhadap provider internal. |
| ConPTY (Windows) bug / latency | Terminal rendering glitch | Test di Windows 10 21H2+ (ConPTY stable). Fallback: winpty. |

---

## 23. Pertanyaan Terbuka

1. **UI Framework final**: Tauri v2 (webview, HTML/CSS/JS frontend)
   vs GPUI (native Rust rendering ala Zed). Tauri lebih mudah untuk
   multi-window dan embedding. Keputusan: **Tauri v2** di v1,
   evaluasi GPUI di v2.

2. **OpenCode versi**: Pastikan versi OpenCode yang dipakai mendukung
   `HTTP_PROXY` environment variable. Perlu testing.

3. **Proxy sourcing**: Apakah Synergy sebaiknya menyertakan
   rekomendasi proxy provider? Atau murni user-provided?

4. **Leader ↔ Worker communication**: Untuk Leader GUI mode, bagaimana
   Leader "membaca" output Worker? Opsi:
   - Orchestrator membaca stdout Worker langsung dan mengirim
     ringkasan ke Leader via UI Automation.
   - Leader punya panel overview terpisah yang menampilkan status
     semua Worker.

5. **Lisensi**: MIT / Apache-2.0 dual license (standar Rust).

---

### Penutup

Dokumen ini mendefinisikan Synergy sebagai **workspace orchestrator
yang mengintegrasikan banyak AI tool (CLI & GUI) dalam satu layar**,
dengan arsitektur Leader (bebas pilih app) + Worker (OpenCode CLI × 6,
IP-isolated via proxy). Arsitektur ini pragmatis: Phase 1 (CLI) bisa
dicapai dalam 6 minggu dan sudah menghasilkan produk yang berfungsi
penuh, sementara Phase 2 (GUI embedding) menambah wow-factor sebagai
fitur premium.
