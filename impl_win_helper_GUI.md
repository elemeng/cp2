# cp2-setup — Windows sshd + firewall setup helper: implementation record

**Implemented 2026-09-03 on a real Windows/MSVC machine.** The `win` module
was rewritten to the windows-sys 0.61 raw-pointer API, `cp2-setup`
compiles on Windows, `cargo clippy --all-targets -- -D warnings` is clean,
and the 6 parser tests pass. This file keeps the verified API shapes and
the decisions already made so follow-up work (UAT, packaging tweaks) needs
no re-derivation. What is genuinely left is UAT on real Windows 10/11 and
CI validation on the next tagged release.

## What the feature is

`cp2-setup.exe` — a small native Win32 GUI helper shipped in the release
tarballs. Double-click → it checks whether the OpenSSH Server (sshd)
feature/service and the inbound firewall rule (TCP 22) are present, shows
the status in a window, and — after the user confirms an explicit action
list — enables the missing pieces elevated (UAC). This removes the
"enable the OpenSSH Server feature + firewall gateway" friction for
non-IT Windows users. It does **not** remove the sshd dependency (that is
the separate russh-listen-mode idea); it makes the existing sshd path
painless.

## Current state (what is verified)

| Item | Status |
|---|---|
| `setup/main.rs` exists, new file | ✅ written |
| `Cargo.toml`: `[[bin]] cp2-setup` + extra windows-sys features | ✅ edited (incl. `Win32_UI_Input_KeyboardAndMouse` — `EnableWindow` lives there in 0.61) |
| Linux stub build | ✅ passes |
| Unit tests (`cargo test --bin cp2-setup`) — 6 parser/status tests | ✅ all pass on Linux and Windows |
| Clippy (`cargo clippy --all-targets -- -D warnings`) | ✅ clean on Windows/MSVC |
| **Windows compile of the `win` module (rewritten to 0.61)** | ✅ compiles on Windows/MSVC x86_64 |
| Packaging (build-release.sh / release.yml / install.sh) | ✅ done |
| Docs (README note / AGENTS.md layout) | ✅ done |
| Real-machine UAT | ⏳ not done |

Everything above is committed in the merge `fcef307` (together with the
setup scaffold itself); the win-module rewrite is part of the follow-up
commit.

## The windows-sys 0.61 API rewrite (done 2026-09-03)

The `win` module was written assuming the windows-sys **0.60-style** API
(handle types as `Option<HWND>` params, `HWND(pub *mut c_void)` tuple
structs); the vendored crate is **windows-sys 0.61.2**, which uses
**raw-pointer type aliases and plain (non-`Option`) parameters**:

- `pub type HWND = *mut core::ffi::c_void;` (same for `HINSTANCE`,
  `HMODULE`, `HMENU`, `HICON`, `HCURSOR`, `HBRUSH`, `HGDIOBJ`, `HANDLE`)
- `WPARAM = usize`, `LPARAM = isize`, `LRESULT = isize`, `BOOL = i32`
- functions take `HWND` values directly — **pass `std::ptr::null_mut()`/raw
  handles, never `None`/`Some(...)`**
- `ShellExecuteW(..., lpparameters: PCWSTR, ...)` — **not** `Option<PCWSTR>`
- `CreateWindowExW(..., hwndparent: HWND, hmenu: HMENU, ..., lpparam:
  *const c_void)` — no options; menu IDs cast `IDC_ENABLE as HMENU`
- `WNDCLASSW.lpfnWndProc: WNDPROC` where
  `WNDPROC = Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>`
  (a safe `extern "system" fn` item coerces to the unsafe fn pointer)
- `MessageBoxW(hwnd: HWND, ...) -> MESSAGEBOX_RESULT` (`i32`; `IDYES = 6`)
- `GetStockObject(GET_STOCK_OBJECT_FLAGS) -> HGDIOBJ`
- `COLOR_WINDOW` lives in **`Win32::Graphics::Gdi`** (type
  `SYS_COLOR_INDEX` = i32), **not** WindowsAndMessaging
- `ES_MULTILINE`/`ES_READONLY`/`ES_AUTOVSCROLL` live in
  **`Win32::UI::WindowsAndMessaging`** (i32 — cast to `u32` to combine with
  `WS_*`), `EM_SETSEL`/`EM_REPLACESEL` in `Win32::UI::Controls` (u32)
- `OpenProcessToken(HANDLE, TOKEN_ACCESS_MASK, *mut HANDLE) -> BOOL`;
  `GetTokenInformation(HANDLE, TOKEN_INFORMATION_CLASS, *mut c_void, u32,
  *mut u32) -> BOOL`; `TokenElevation = 20`, `TOKEN_QUERY = 8`

What changed in the rewrite:
1. Every `Option<HWND>`/`Some(...)`/`None` call site and every `.0` field
   access (`hwnd.0.is_null()` → `hwnd.is_null()`,
   `r.0 > 32` → `r as usize > 32`, `font.0 as usize` → `font as usize`)
   was rewritten.
2. **`HWND` (a raw pointer) is not `Send`** — the worker thread moves the
   window handle into `std::thread::spawn`. Wrapped as
   `#[derive(Clone, Copy)] struct Hwnd(HWND); unsafe impl Send for Hwnd {}`,
   stored in the `Ui` struct so `static UI: Mutex<Option<Ui>>` stays valid.
   **Gotcha:** closures capture field-precisely, so `hwnd.0` inside the
   worker closure captures just the raw pointer (not `Send`). The closure
   must use a `Hwnd::post(self, ...)` method (receiver by value forces
   whole-struct capture).
3. `ShellExecuteW` success check: `r as usize > 32`.
4. `GetModuleHandleW(std::ptr::null())` returns `HMODULE` (same alias type
   as `HINSTANCE`) — usable directly.
5. Miscellaneous compile fixes discovered on the first real Windows build:
   `EnableWindow` lives in **`Win32::UI::Input::KeyboardAndMouse`** (was
   unresolved; added the `Win32_UI_Input_KeyboardAndMouse` feature);
   `HBRUSH` is a `Gdi` alias, not a `Foundation` tuple struct;
   `encode_wide` needs `std::ffi::OsStr` + `OsStrExt`, not `&str`; and the
   `Ui` struct was missing the `enable` field the code used.
6. Clippy (pedantic) fixes: `if !x { action } else { note }` inverted to
   `if x { note } else { action }` (`if_not_else`), explicit
   `std::ptr::from_mut/from_ref` instead of implicit `&mut`→`*mut` pointer
   coercions, `u32::try_from(size_of::<TOKEN_ELEVATION>())` instead of a
   truncating `as u32`, and a single `Box::into_raw(boxed) as isize` cast.

Everything else in the file was deliberately verified against the vendored
source (see "Verified API shapes" in `~/.cargo/registry/src/*/windows-sys-0.61.2/`).

## Architecture (decisions already made — do not re-litigate)

- **Single file layout:** `setup/main.rs` outside `src/` — build.rs hashes
  only `src/**`, so the wire-protocol fingerprint (`CP2_BUILD_FINGERPRINT`)
  stays untouched by GUI edits. Second `[[bin]]` added to Cargo.toml.
- **Platform split:** `core_logic` module (status model + pure parsers) is
  platform-independent and unit-tested everywhere; only the `win` module
  (spawns + GUI) is `#[cfg(windows)]`; non-Windows builds get a stub
  `main` that prints "Windows-only".
- **Probe = locale-free by construction** (parsers in `core_logic`):
  - sshd feature/service state: `sc query sshd` → exit code `1060`
    (`ERROR_SERVICE_DOES_NOT_EXIST`) means not installed; else parse the
    numeric `STATE : N` (4 = RUNNING) and `sc qc sshd` → numeric
    `START_TYPE : N` (2 = AUTO_START). Numbers are locale-independent.
  - firewall rule: `reg query "HKLM\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules" /v OpenSSH-Server-In-TCP`
    → **exit code only** (0 = rule exists). Chosen over netsh because
    netsh's "No rules match" is localized. **Verify on real Windows** that
    a missing rule yields a nonzero exit code (assumed, not yet confirmed).
- **Enable steps** (elevated, re-probed before each step, idempotent):
  1. feature: `dism.exe /online /add-capability /capabilityname:OpenSSH.Server~~~~0.0.1.0`
     (exit 0 or 3010 = success; 3010 = reboot requested → defer service step)
  2. service: `sc.exe config sshd start= auto` then `sc.exe start sshd`
     (exit 1056 = already running, treat as success)
  3. firewall: `netsh.exe advfirewall firewall delete rule name=OpenSSH-Server-In-TCP`
     (ignore its exit code) **then** `add rule name=OpenSSH-Server-In-TCP
     dir=in action=allow protocol=TCP localport=22 profile=any`
     (delete-then-add avoids "already exists")
- **Elevation model:** the GUI runs unprivileged; checks need no admin.
  Enable → MessageBox with the numbered action list → if not elevated,
  relaunch self via `ShellExecuteW(runas, "--enable")` and exit; the
  elevated twin runs the same window, skips the confirm, and executes.
- **Threading:** enable steps run on a `std::thread`; progress lines are
  posted to the window via `PostMessageW(WM_APP_APPEND, Box<String>)` and
  completion via `WM_APP_DONE` (which re-probes + re-enables the button).
  Handles must never be used off the main thread except as `PostMessageW`
  values.
- **UI:** ~590×440 Win32 window, one multiline read-only EDIT (status) +
  "Enable missing components" + "Close" buttons, `DEFAULT_GUI_FONT`
  (`GetStockObject`), `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)`
  best-effort. **ASCII-only status markers** (`[OK]`, `[MISSING]`,
  `[STOPPED]`, `[DEMAND]`) — the default GUI font has no ⚠/✔ glyphs.
- **Command-injection posture (Mimosa-enforced):** every spawn is
  `std::process::Command::new("literal").args([...literals])` inlined at
  the call site — no helper that takes program/args parameters, no
  dynamic data in any argv. Keep this style when editing; `output_of()` is
  the only shared helper (it only folds an `io::Result<Output>`).
- **Windows build behavior to know:** `cargo build --bin cp2-setup` on a
  Windows target also compiles the package lib (and therefore the whole
  russh/aws-lc tree, since `[dependencies]` are package-wide) — that is
  expected; cp2-setup itself only links windows-sys + std.

## Verified API shapes (windows-sys 0.61.2, from the vendored crate)

All under `windows_sys::Win32::…`; feature names in Cargo.toml are already
added: `Win32_UI_WindowsAndMessaging`, `Win32_UI_Controls`, `Win32_UI_Shell`,
`Win32_UI_HiDpi`, `Win32_Graphics_Gdi`, `Win32_Security`,
`Win32_System_Threading`, `Win32_System_LibraryLoader`.

| API | Signature (0.61.2) |
|---|---|
| `GetModuleHandleW` (LibraryLoader) | `(PCWSTR) -> HMODULE` (HMODULE = `*mut c_void`) |
| `ShellExecuteW` (UI::Shell) | `(HWND, PCWSTR op, PCWSTR file, PCWSTR params, PCWSTR dir, SHOW_WINDOW_CMD) -> HINSTANCE` |
| `CreateWindowExW` | `(WINDOW_EX_STYLE, PCWSTR, PCWSTR, WINDOW_STYLE, i32×4, HWND, HMENU, HINSTANCE, *const c_void) -> HWND` |
| `MessageBoxW` | `(HWND, PCWSTR, PCWSTR, MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT` (i32) |
| `RegisterClassW` | `(*const WNDCLASSW) -> u16` |
| `DefWindowProcW` / `SendMessageW` / `PostMessageW` | `(HWND, u32, WPARAM, LPARAM) -> LRESULT` |
| `GetMessageW` | `(*mut MSG, HWND, u32, u32) -> BOOL` |
| `PostQuitMessage` | `(i32)` — no return |
| `SetWindowTextW` | `(HWND, PCWSTR) -> BOOL` |
| `EnableWindow` | `(HWND, BOOL) -> BOOL` (grep found no `EnableWindow` in WindowsAndMessaging — double-check its home when resuming; may need `Win32_UI_WindowsAndMessaging::EnableWindow` or it lives elsewhere) |
| `ShowWindow` | `(HWND, SHOW_WINDOW_CMD) -> BOOL` (`SW_SHOWNORMAL = 1`) |
| `LoadIconW` / `LoadCursorW` | `(HINSTANCE, PCWSTR) -> HICON` / `(HINSTANCE, PCWSTR) -> HCURSOR` (`IDI_APPLICATION`/`IDC_ARROW` are PCWSTR consts, pass null hinstance for cursor) |
| `GetStockObject` (Graphics::Gdi) | `(GET_STOCK_OBJECT_FLAGS) -> HGDIOBJ` (`DEFAULT_GUI_FONT = 17`) |
| `SetProcessDpiAwarenessContext` (UI::HiDpi) | `(DPI_AWARENESS_CONTEXT) -> BOOL` (`DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4 as _`) |
| `OpenProcessToken` (System::Threading) | `(HANDLE, TOKEN_ACCESS_MASK, *mut HANDLE) -> BOOL`; `TOKEN_QUERY = 8` |
| `GetTokenInformation` (Security) | `(HANDLE, TOKEN_INFORMATION_CLASS, *mut c_void, u32, *mut u32) -> BOOL`; `TokenElevation = 20`; `TOKEN_ELEVATION { TokenIsElevated: u32 }` |
| `GetCurrentProcess` (System::Threading) | `() -> HANDLE` |
| WNDCLASSW | `{ style: WNDCLASS_STYLES, lpfnWndProc: WNDPROC, cbClsExtra: i32, cbWndExtra: i32, hInstance: HINSTANCE, hIcon: HICON, hCursor: HCURSOR, hbrBackground: HBRUSH, lpszMenuName: PCWSTR, lpszClassName: PCWSTR }` (derives `Default` with Win32_Graphics_Gdi) |
| Consts | `WS_*`/`WS_EX_CLIENTEDGE`/`CS_*`/`WM_SETFONT=48`/`WM_APP=32768` in WindowsAndMessaging; `ES_*` in WindowsAndMessaging (i32); `EM_*` in Controls (u32); `MB_*` (MESSAGEBOX_STYLE=u32) + `IDYES=6` in WindowsAndMessaging; `COLOR_WINDOW=5` in Graphics::Gdi (SYS_COLOR_INDEX) |

## Remaining work

Done since this file was first written: the `win` module rewrite (1),
the MSVC compile check (2 — ran `cargo build --release --bin cp2-setup`
on this Windows machine), clippy `-D warnings` on Windows (3), the
packaging (4, all three scripts), and the docs (5, README + AGENTS.md).

Still open:

1. **Real-machine UAT checklist** (Windows 10/11):
   - fresh box without OpenSSH Server: status shows [MISSING] ×2; Enable →
     UAC → DISM installs (~minutes) → service auto-starts → rule added →
     "All set"; then verify `cp2 user@thatbox:...` push/pull works.
   - box with feature already enabled: all [OK], Enable button says
     everything is in place.
   - localized (non-English) Windows: status still correct (numbers/exit
     codes only).
   - UAC cancel path: nothing changes, app stays open.
   - Verify `reg query ... /v OpenSSH-Server-In-TCP` exit code when the rule
     is missing (currently assumed nonzero).
   - Verify `sc query sshd` exit 1060 on a feature-less machine, and
     `sc start sshd` 1056-on-already-running behavior.
2. **CI validation** — the next tagged release exercises the new
   release.yml cp2-setup rows (x86_64 msvc native + aarch64 zig cross) and
   the test job's `cargo test --all-targets` / clippy on all three runners.

## Behavior notes / gotchas recorded during implementation

- `EM_REPLACESEL` works on `ES_READONLY` edits when sent programmatically
  (the read-only style only blocks user input) — used for appending status.
- The worker appends via `EM_SETSEL(-1,-1)` + `EM_REPLACESEL`; the boxed
  `String` in `WM_APP_APPEND`'s lparam is created/leaked by the worker and
  reclaimed (`Box::from_raw`) by the message loop.
- DISM output text is shown only on the failure path (last few lines);
  success is judged by exit code (0 / 3010) since the text is localized.
- The elevated twin re-probes per step, so the plan is always current even
  if the machine changed between the confirm and the run.
- `cargo test --all-targets` on Linux compiles the stub + runs the 6
  parser/status tests; keep parsers in `core_logic` (ungated) so they stay
  testable cross-platform.
- Mimosa PreToolUse hook blocks writes that look like command-building
  helpers — keep the literal-only inline-spawn style it accepted.