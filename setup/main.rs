//! cp2-setup — the one-window Windows helper for the sshd + firewall pieces
//! a cp2 sync needs on the receiving machine.
//!
//! Double-click it: it checks whether the OpenSSH Server (sshd) feature is
//! installed and its service running, and whether the inbound firewall rule
//! for port 22 exists, and shows the status. When something is missing, the
//! Enable button lists the exact actions in a confirmation prompt and runs
//! them elevated (`--enable`; the UAC prompt comes from a `runas` relaunch
//! of this same binary). The checks themselves need no administrator rights;
//! only the Enable step elevates.
//!
//! The probe reads only exit codes and machine-readable numbers — never
//! localized text — so the status is correct on non-English Windows too.
//!
//! This binary is the Windows-side companion to `cp2`; on other platforms it
//! only explains itself.
#![cfg_attr(windows, windows_subsystem = "windows")]

/// Platform-independent core: the status model and the pure output parsers.
/// Kept outside the `cfg(windows)` gate so the parsing logic is unit-tested
/// on every CI platform; only the command spawns and the GUI are gated.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
mod core_logic {
    use std::fmt::Write as _;
    /// The OpenSSH server's service state on this machine.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SshdState {
        /// `sc query sshd` failed with `ERROR_SERVICE_DOES_NOT_EXIST`.
        NotInstalled,
        /// The service exists; `running` is the RUNNING state, `auto_start`
        /// the configured start type (`AUTO_START`).
        Installed {
            running: bool,
            auto_start: bool,
        },
    }

    /// What the probe found on this machine.
    #[derive(Debug, Clone)]
    pub struct SystemStatus {
        pub sshd: SshdState,
        /// The inbound firewall rule `OpenSSH-Server-In-TCP` exists.
        pub firewall_rule: bool,
    }

    impl SystemStatus {
        /// Human-readable problems; empty means the machine is ready.
        #[must_use]
        pub fn problems(&self) -> Vec<&'static str> {
            let mut v = Vec::new();
            match self.sshd {
                SshdState::NotInstalled => {
                    v.push("OpenSSH Server (sshd) is not installed");
                }
                SshdState::Installed {
                    running,
                    auto_start,
                } => {
                    if !auto_start {
                        v.push("the sshd service does not start automatically");
                    }
                    if !running {
                        v.push("the sshd service is not running");
                    }
                }
            }
            if !self.firewall_rule {
                v.push("no inbound firewall rule for TCP port 22");
            }
            v
        }

        #[must_use]
        pub fn ready(&self) -> bool {
            self.problems().is_empty()
        }
    }

    /// Windows service states as printed by `sc query` (the keyword after the
    /// number is localized; the number is not): 1 `STOPPED`, 2 `START_PENDING`,
    /// 3 `STOP_PENDING`, 4 `RUNNING`, 5 `CONTINUE_PENDING`, 6 `PAUSE_PENDING`,
    /// 7 `PAUSED`.
    #[must_use]
    pub fn sc_running_state(out: &str) -> Option<u32> {
        for line in out.lines() {
            let mut words = line.split_whitespace();
            if words.next() == Some("STATE") {
                words.next(); // the ':'
                return words.next().and_then(|n| n.parse().ok());
            }
        }
        None
    }

    /// The configured start type from `sc qc sshd` (`START_TYPE : 2
    /// AUTO_START`); 2 is `AUTO_START`.
    #[must_use]
    pub fn sc_start_type(out: &str) -> Option<u32> {
        for line in out.lines() {
            let mut words = line.split_whitespace();
            if words.next() == Some("START_TYPE") {
                words.next(); // the ':'
                return words.next().and_then(|n| n.parse().ok());
            }
        }
        None
    }

    /// `sc` start types: 2 = `AUTO_START`, 3 = `DEMAND_START`.
    pub const SC_START_AUTO: u32 = 2;
    /// Exit code of `sc query sshd` when the service does not exist.
    pub const SC_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    /// Exit code of `sc start sshd` when the service is already running.
    pub const SC_ALREADY_RUNNING: i32 = 1056;
    /// `DISM` exit code for success-with-reboot-requested (also a success).
    pub const DISM_REBOOT_REQUIRED: i32 = 3010;

    /// The status report shown in the window. ASCII markers only — the
    /// default GUI font carries no ⚠/✔ glyphs.
    #[must_use]
    pub fn status_report(s: &SystemStatus) -> String {
        let mut out = String::new();
        out.push_str("cp2 - Windows SSH server setup\n");
        out.push_str("================================\n\n");
        match s.sshd {
            SshdState::NotInstalled => {
                out.push_str("[MISSING] OpenSSH Server (sshd) feature\n");
                out.push_str("          (service not present: 'sc query sshd')\n");
            }
            SshdState::Installed {
                running,
                auto_start,
            } => {
                let _ = writeln!(
                    out,
                    "[{}] OpenSSH Server (sshd) feature",
                    if running { "OK     " } else { "STOPPED" }
                );
                let _ = writeln!(
                    out,
                    "[{}] sshd starts automatically",
                    if auto_start { "OK     " } else { "DEMAND " }
                );
            }
        }
        let _ = writeln!(
            out,
            "[{}] firewall rule OpenSSH-Server-In-TCP (TCP 22)",
            if s.firewall_rule {
                "OK     "
            } else {
                "MISSING"
            }
        );
        out.push('\n');
        if s.ready() {
            out.push_str("All set - this machine can receive cp2 syncs.\n");
            out.push_str("Press Close. (No changes were made.)\n");
        } else {
            let _ = writeln!(
                out,
                "Needs attention: {} problem(s). Press 'Enable missing components'.",
                s.problems().len()
            );
        }
        out
    }

    /// The actions the Enable step would take, derived from the current
    /// status and listed verbatim in the confirmation prompt.
    #[must_use]
    pub fn planned_actions(s: &SystemStatus) -> Vec<&'static str> {
        let mut v = Vec::new();
        match s.sshd {
            SshdState::NotInstalled => {
                v.push("Enable the OpenSSH Server feature via DISM\n        (needs Windows Update; may take a few minutes)");
            }
            SshdState::Installed {
                running,
                auto_start,
            } => {
                if !auto_start {
                    v.push("Set the sshd service to start automatically");
                }
                if !running {
                    v.push("Start the sshd service");
                }
            }
        }
        if !s.firewall_rule {
            v.push("Add the firewall rule 'OpenSSH-Server-In-TCP'\n        (TCP port 22, inbound, all profiles)");
        }
        v
    }

    /// The confirmation prompt text: the exact plan, then the elevation note.
    #[must_use]
    pub fn confirm_text(s: &SystemStatus) -> String {
        let mut out = String::from(
            "cp2-setup will run with administrator rights and do exactly this:\n\n",
        );
        for (i, a) in planned_actions(s).iter().enumerate() {
            let _ = writeln!(out, "{}. {a}", i + 1);
        }
        out.push_str("\nA UAC prompt follows. You can cancel it; nothing changes then.\n");
        out
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "cp2-setup is a Windows-only utility: it checks and enables the OpenSSH \
         Server feature and the firewall rule a cp2 sync needs on the receiving \
         machine. Run it on Windows."
    );
    std::process::exit(1);
}

#[cfg(windows)]
mod win {
    use super::core_logic::{
        confirm_text, status_report, sc_running_state, sc_start_type, DISM_REBOOT_REQUIRED,
        SC_ALREADY_RUNNING, SC_SERVICE_DOES_NOT_EXIST, SC_START_AUTO, SshdState, SystemStatus,
    };
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{GetStockObject, COLOR_WINDOW, DEFAULT_GUI_FONT};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::Controls::{EM_REPLACESEL, EM_SETSEL};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, DefWindowProcW,
        DispatchMessageW, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, GetMessageW,
        HMENU, IDC_ARROW, IDI_APPLICATION, IDYES, LoadCursorW, LoadIconW, MB_DEFBUTTON2,
        MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, MessageBoxW, PostMessageW,
        PostQuitMessage, RegisterClassW, SendMessageW, SetWindowTextW, ShowWindow,
        SW_SHOWNORMAL, TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND, WM_SETFONT,
        WNDCLASSW, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
        WS_VISIBLE, WS_VSCROLL,
    };

    /// Fold a captured `Output` into (exit code, stdout+stderr text). Exit
    /// code -1 means the program could not be started at all. Every spawn in
    /// this app passes string literals only — nothing user-, file-, or
    /// environment-derived ever reaches a command line, and each argument
    /// travels as its own argv element with no shell involved.
    #[must_use]
    fn output_of(out: std::io::Result<std::process::Output>) -> (i32, String) {
        match out {
            Ok(out) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                (out.status.code().unwrap_or(-1), text)
            }
            Err(e) => (-1, format!("failed to spawn: {e}")),
        }
    }

    /// The wide NUL-terminated form Windows APIs want.
    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    // ---- probe --------------------------------------------------------------

    /// Check sshd feature/service state and the firewall rule. Only exit
    /// codes and machine-readable numbers are parsed (see the parsers in
    /// `core_logic`), so a localized Windows reports the same status.
    #[must_use]
    pub fn probe() -> SystemStatus {
        let (code, out) = output_of(
            std::process::Command::new("sc.exe").args(["query", "sshd"]).output(),
        );
        let sshd = if code == SC_SERVICE_DOES_NOT_EXIST || code == -1 {
            SshdState::NotInstalled
        } else {
            let (qc, qout) = output_of(
                std::process::Command::new("sc.exe").args(["qc", "sshd"]).output(),
            );
            SshdState::Installed {
                running: sc_running_state(&out) == Some(4),
                auto_start: qc == 0 && sc_start_type(&qout) == Some(SC_START_AUTO),
            }
        };
        // The firewall store keeps one value per rule, named after the rule
        // (`FirewallRules` under SharedAccess). `reg query ... /v NAME`
        // answers via its exit code in every locale, unlike netsh's
        // localized "No rules match" text.
        let (fcode, _) = output_of(
            std::process::Command::new("reg.exe")
                .args([
                    "query",
                    "HKLM\\SYSTEM\\CurrentControlSet\\Services\\SharedAccess\\Parameters\\FirewallPolicy\\FirewallRules",
                    "/v",
                    "OpenSSH-Server-In-TCP",
                ])
                .output(),
        );
        let firewall_rule = fcode == 0;
        SystemStatus { sshd, firewall_rule }
    }

    // ---- elevation -----------------------------------------------------------

    /// Whether this process runs with an elevated (administrator) token.
    #[must_use]
    pub fn is_elevated() -> bool {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: GetCurrentProcess returns a pseudo-handle of the current
        // process; OpenProcessToken writes the real token handle only on
        // success (return value != 0).
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, std::ptr::from_mut(&mut token)) };
        if ok == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        // SAFETY: `elevation` is writable for the requested size and `len`
        // receives the written byte count; the token handle is a kernel
        // handle our process owns and the OS closes it at exit.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                std::ptr::from_mut(&mut elevation).cast::<core::ffi::c_void>(),
                u32::try_from(std::mem::size_of::<TOKEN_ELEVATION>())
                    .expect("TOKEN_ELEVATION is 4 bytes"),
                std::ptr::from_mut(&mut len),
            )
        };
        ok != 0 && elevation.TokenIsElevated != 0
    }

    /// Relaunch this binary elevated ("runas" → the UAC prompt). The only
    /// argument ever passed is the fixed `--enable` flag.
    fn relaunch_elevated() -> std::io::Result<()> {
        let exe = std::env::current_exe()?;
        let exe_w = wide(&exe.to_string_lossy());
        let args_w = wide("--enable");
        let runas = wide("runas");
        // SAFETY: all three strings are NUL-terminated wide strings; "runas"
        // with a null parent window shows the UAC prompt. A return value > 32
        // is a successful spawn (HINSTANCE is a fake "instance" handle for
        // ShellExecute, interpreted as an error code otherwise).
        let r = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                runas.as_ptr(),
                exe_w.as_ptr(),
                args_w.as_ptr(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if r as usize > 32 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    // ---- the Enable steps (elevated) -----------------------------------------

    /// Run the missing pieces, re-probing before each step (idempotent: a
    /// machine that changed since the window opened is handled correctly).
    /// Every outcome — including failures — is reported through `report`.
    pub fn enable_all(report: &mut dyn FnMut(&str)) {
        let mut state = probe();
        let mut reboot_pending = false;
        if matches!(state.sshd, SshdState::NotInstalled) {
            report("[1/3] enabling the OpenSSH Server feature (DISM)...");
            let (code, out) = output_of(
                std::process::Command::new("dism.exe")
                    .args([
                        "/online",
                        "/add-capability",
                        "/capabilityname:OpenSSH.Server~~~~0.0.1.0",
                    ])
                    .output(),
            );
            if code == 0 || code == DISM_REBOOT_REQUIRED {
                if code == DISM_REBOOT_REQUIRED {
                    report(
                        "[1/3] feature enabled; DISM requests a reboot before the\n        service can start. Reboot, then run this tool again.",
                    );
                    reboot_pending = true;
                } else {
                    report("[1/3] OpenSSH Server feature enabled.");
                }
                state = probe();
            } else {
                report(&format!("[1/3] FAILED to enable the feature (exit {code})."));
                report("        Last lines of the output:");
                let tail: Vec<_> = out.lines().rev().take(4).collect();
                for line in tail.iter().rev() {
                    report(line);
                }
                report("        Fix the error above, or enable it in Windows");
                report("        Settings > Apps > Optional features.");
                return;
            }
        }
        match state.sshd {
            SshdState::Installed {
                running,
                auto_start,
            } => {
                if reboot_pending {
                    report("[2/3] skipped: sshd service starts after the reboot.");
                } else {
                    if !auto_start {
                        report("[2/3] setting the sshd service to start automatically...");
                        let (code, _) = output_of(
                            std::process::Command::new("sc.exe")
                                .args(["config", "sshd", "start=", "auto"])
                                .output(),
                        );
                        if code == 0 {
                            report("        done.");
                        } else {
                            report(&format!("[2/3] sc config failed (exit {code})."));
                        }
                    }
                    if running {
                        report("[2/3] sshd service already running.");
                    } else {
                        report("[2/3] starting the sshd service...");
                        let (code, out) = output_of(
                            std::process::Command::new("sc.exe")
                                .args(["start", "sshd"])
                                .output(),
                        );
                        if code == 0 || code == SC_ALREADY_RUNNING {
                            report("        done.");
                        } else {
                            report(&format!(
                                "[2/3] sc start failed (exit {code}): {}",
                                out.lines().last().unwrap_or("")
                            ));
                            report("        The service account or host keys may need");
                            report("        attention.");
                        }
                    }
                }
            }
            SshdState::NotInstalled => {
                report("[2/3] skipped: no sshd service (feature step failed above).");
            }
        }
        if state.firewall_rule {
            report("[3/3] firewall rule already present.");
        } else {
            // Delete-then-add: a rule with the same name would otherwise make
            // netsh fail with "already exists"; deleting a missing rule is a
            // non-fatal error we ignore. The add is what matters.
            report("[3/3] adding the firewall rule 'OpenSSH-Server-In-TCP'...");
            let _ = output_of(
                std::process::Command::new("netsh.exe")
                    .args(["advfirewall", "firewall", "delete", "rule", "name=OpenSSH-Server-In-TCP"])
                    .output(),
            );
            let (code, out) = output_of(
                std::process::Command::new("netsh.exe")
                    .args([
                        "advfirewall",
                        "firewall",
                        "add",
                        "rule",
                        "name=OpenSSH-Server-In-TCP",
                        "dir=in",
                        "action=allow",
                        "protocol=TCP",
                        "localport=22",
                        "profile=any",
                    ])
                    .output(),
            );
            if code == 0 {
                report("        done.");
            } else {
                report(&format!("[3/3] FAILED (exit {code})."));
                let tail: Vec<_> = out.lines().rev().take(3).collect();
                for line in tail.iter().rev() {
                    report(line);
                }
                report("        Add it manually: Windows Defender Firewall >");
                report("        Advanced settings > Inbound rules > New rule.");
            }
        }
        report("Re-checking...");
        final_report(report, &probe());
    }

    /// The closing lines after the final re-probe, with per-step hints.
    fn final_report(report: &mut dyn FnMut(&str), s: &SystemStatus) {
        if s.ready() {
            report("");
            report("All set. This machine can now receive cp2 syncs.");
            return;
        }
        report("");
        report("Still open:");
        for p in s.problems() {
            report(&format!("  - {p}"));
        }
        if matches!(s.sshd, SshdState::Installed { running: false, .. }) {
            report("  (if the feature was just installed, a reboot may be needed)");
        }
    }

    // ---- the window -----------------------------------------------------------

    const IDC_ENABLE: usize = 1001;
    const IDC_CLOSE: usize = 1002;
    const WM_APP_APPEND: u32 = WM_APP + 1;
    const WM_APP_DONE: u32 = WM_APP + 2;

    /// `HWND` is a raw `*mut c_void`, which is not `Send`; the worker thread
    /// only ever passes the value to `PostMessageW`, so a `Send` wrapper is
    /// sound. Stored in `Ui` so the `static UI` stays valid.
    #[derive(Clone, Copy)]
    struct Hwnd(HWND);
    unsafe impl Send for Hwnd {}

    impl Hwnd {
        /// Post a message to the window from the worker thread. Taking `self`
        /// by value also makes the worker closure capture the whole `Hwnd`
        /// (which is `Send`) instead of just the raw handle field.
        fn post(self, msg: u32, wparam: WPARAM, lparam: LPARAM) {
            // SAFETY: `self.0` is our own window's handle, valid for the
            // process lifetime; PostMessageW only queues the message.
            unsafe { PostMessageW(self.0, msg, wparam, lparam) };
        }
    }

    /// The controls the window must talk to; the worker thread only posts
    /// messages, so everything else happens on the message-loop thread.
    struct Ui {
        edit: Hwnd,
        enable: Hwnd,
        status: SystemStatus,
        elevated: bool,
    }

    static UI: Mutex<Option<Ui>> = Mutex::new(None);

    fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> R {
        f(UI.lock().unwrap().as_mut().expect("UI initialized"))
    }

    fn append_text(edit: HWND, text: &str) {
        let wide = wide(text);
        // SAFETY: synchronous SendMessage calls on our own EDIT handle; the
        // wide buffer stays alive for both calls. EM_SETSEL(-1, -1) moves
        // the caret to the end so EM_REPLACESEL appends.
        unsafe {
            SendMessageW(edit, EM_SETSEL, usize::MAX, -1);
            SendMessageW(edit, EM_REPLACESEL, 0, wide.as_ptr() as isize);
        }
    }

    fn set_text(hwnd: HWND, text: &str) {
        let wide = wide(text);
        // SAFETY: `wide` is NUL-terminated for the duration of the call.
        unsafe { SetWindowTextW(hwnd, wide.as_ptr()) };
    }

    #[must_use]
    fn message_box(text: &str, title: &str, style: u32) -> i32 {
        let text = wide(text);
        let title = wide(title);
        // SAFETY: both strings are NUL-terminated for the call; the message
        // box is a modal dialog, so the transient buffers are fine.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), style) }
    }

    /// The Enable button: present the exact plan, then say whether the user
    /// confirmed. The elevated instance runs it; a non-elevated instance
    /// relaunches itself elevated instead.
    fn decide_enable() -> bool {
        let status = with_ui(|ui| ui.status.clone());
        if status.ready() {
            let _ = message_box(
                "Everything is already in place.",
                "cp2-setup",
                MB_OK | MB_ICONINFORMATION,
            );
            return false;
        }
        let prompt = confirm_text(&status);
        let title = wide("cp2-setup - confirm");
        let text = wide(&prompt);
        // SAFETY: both strings are NUL-terminated; MB_YESNO returns IDYES
        // (6) on Yes, IDNO (7) on No/Cancel.
        let yes = unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
            )
        };
        yes == IDYES
    }

    fn start_worker(hwnd: HWND) {
        let (edit, enable) = with_ui(|ui| (ui.edit, ui.enable));
        // SAFETY: EnableWindow on our own child button.
        unsafe { EnableWindow(enable.0, 0) };
        append_text(edit.0, "\nWorking... (the feature step can take minutes)\n");
        // The worker thread only posts messages with this handle.
        let hwnd = Hwnd(hwnd);
        std::thread::spawn(move || {
            let mut report = |line: &str| {
                let boxed = Box::new(line.to_string());
                // SAFETY: `boxed` is leaked here and reclaimed by the message
                // loop when it handles WM_APP_APPEND (Box::from_raw).
                hwnd.post(
                    WM_APP_APPEND,
                    0,
                    Box::into_raw(boxed) as isize,
                );
            };
            enable_all(&mut report);
            // SAFETY: posts to our own window; WM_APP_DONE carries no data.
            hwnd.post(WM_APP_DONE, 0, 0);
        });
    }

    extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
        match msg {
            WM_COMMAND if (wparam & 0xFFFF) == IDC_ENABLE => {
                if decide_enable() {
                    let elevated = with_ui(|ui| ui.elevated);
                    if elevated {
                        append_text(ui_controls_edit(), "\n");
                        start_worker(hwnd);
                    } else {
                        match relaunch_elevated() {
                            Ok(()) => {
                                // SAFETY: terminates this instance's loop;
                                // the elevated twin takes over.
                                unsafe { PostQuitMessage(0) };
                            }
                            Err(e) => {
                                let _ = message_box(
                                    &format!(
                                        "Could not start the elevated copy: {e}\n\
                                         Try right-clicking cp2-setup.exe and 'Run as administrator'."
                                    ),
                                    "cp2-setup",
                                    MB_OK | MB_ICONWARNING,
                                );
                            }
                        }
                    }
                }
                0
            }
            WM_COMMAND if (wparam & 0xFFFF) == IDC_CLOSE => {
                // SAFETY: terminates our own message loop.
                unsafe { PostQuitMessage(0) };
                0
            }
            WM_APP_APPEND => {
                // SAFETY: the worker created this Box<String> and leaked it
                // into lparam; we take it back, append, and free it.
                let s = unsafe { Box::from_raw(lparam as *mut String) };
                append_text(ui_controls_edit(), &s);
                0
            }
            WM_APP_DONE => {
                let status = probe();
                with_ui(|ui| {
                    ui.status = status;
                    // SAFETY: EnableWindow on our own child button.
                    unsafe { EnableWindow(ui.enable.0, 1) };
                });
                let st = with_ui(|ui| ui.status.clone());
                append_text(ui_controls_edit(), "\n");
                append_text(ui_controls_edit(), &status_report(&st));
                0
            }
            WM_CLOSE => {
                // SAFETY: terminates our own message loop; WM_DESTROY follows.
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => {
                // SAFETY: the default window procedure handles the message.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
    }

    fn ui_controls_edit() -> HWND {
        with_ui(|ui| ui.edit.0)
    }

    fn gui_main(run_enable: bool) {
        // SAFETY: SetProcessDpiAwarenessContext is best-effort (returns 0 on
        // systems that predate it); ignoring the result keeps the window
        // usable on every Windows version.
        unsafe {
            windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }

        // SAFETY: GetModuleHandleW(None) returns this module's handle.
        let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class_name = wide("Cp2SetupWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            // SAFETY: stock icon/cursor handles never need freeing.
            hIcon: unsafe { LoadIconW(hinstance, IDI_APPLICATION) },
            hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
            // The (COLOR_WINDOW + 1) convention: a system color brush.
            hbrBackground: (COLOR_WINDOW + 1) as *mut core::ffi::c_void,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        // SAFETY: `wc` is fully initialized and lives for the registration.
        unsafe { RegisterClassW(std::ptr::from_ref(&wc)) };

        let title = wide("cp2-setup - Windows SSH server");
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                590,
                440,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            let _ = message_box(
                "Could not create the setup window.",
                "cp2-setup",
                MB_OK | MB_ICONWARNING,
            );
            return;
        }

        let edit = unsafe {
            CreateWindowExW(
                WS_EX_CLIENTEDGE,
                wide("EDIT").as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32,
                16,
                16,
                558,
                330,
                hwnd,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            )
        };
        let enable = unsafe {
            CreateWindowExW(
                0,
                wide("BUTTON").as_ptr(),
                wide("Enable missing components").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                16,
                368,
                230,
                34,
                hwnd,
                IDC_ENABLE as HMENU,
                hinstance,
                std::ptr::null(),
            )
        };
        let close = unsafe {
            CreateWindowExW(
                0,
                wide("BUTTON").as_ptr(),
                wide("Close").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                262,
                368,
                100,
                34,
                hwnd,
                IDC_CLOSE as HMENU,
                hinstance,
                std::ptr::null(),
            )
        };

        // SAFETY: GetStockObject is infallible for a stock font; WM_SETFONT
        // lets the control copy the handle for its own use.
        unsafe {
            let font = GetStockObject(DEFAULT_GUI_FONT);
            SendMessageW(edit, WM_SETFONT, font as usize, 1);
            SendMessageW(enable, WM_SETFONT, font as usize, 1);
            SendMessageW(close, WM_SETFONT, font as usize, 1);
        }

        let status = probe();
        let report = status_report(&status);
        *UI.lock().unwrap() = Some(Ui {
            edit: Hwnd(edit),
            enable: Hwnd(enable),
            status,
            elevated: is_elevated(),
        });
        set_text(edit, &report);
        // SAFETY: the window was created with WS_VISIBLE but a final show
        // guarantees the first paint happens after the status text is in.
        unsafe { ShowWindow(hwnd, SW_SHOWNORMAL) };

        if run_enable {
            // The elevated twin of a confirmed run: execute immediately
            // (each step re-probes, so the plan is still accurate).
            start_worker(hwnd);
        }

        // SAFETY: the standard message pump; GetMessageW returns 0 only on
        // WM_QUIT (which our handlers post).
        unsafe {
            let mut msg = std::mem::zeroed();
            while GetMessageW(std::ptr::from_mut(&mut msg), std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(std::ptr::from_ref(&msg));
                DispatchMessageW(std::ptr::from_ref(&msg));
            }
        }
    }

    pub fn main() {
        let run_enable = std::env::args().any(|a| a == "--enable");
        if run_enable && !is_elevated() {
            // `cp2-setup --enable` from a non-elevated shell: the normal
            // path goes through the GUI's runas relaunch; be explicit here.
            let _ = message_box(
                "cp2-setup --enable must run elevated.\n\
                 Double-click cp2-setup.exe and press 'Enable missing components'.",
                "cp2-setup",
                MB_OK | MB_ICONWARNING,
            );
            std::process::exit(1);
        }
        gui_main(run_enable);
    }
}

#[cfg(windows)]
fn main() {
    win::main();
}

#[cfg(test)]
mod tests {
    use super::core_logic::{
        confirm_text, planned_actions, sc_running_state, sc_start_type, status_report,
        SshdState, SystemStatus, SC_START_AUTO,
    };

    fn status(sshd: SshdState, firewall_rule: bool) -> SystemStatus {
        SystemStatus {
            sshd,
            firewall_rule,
        }
    }

    #[test]
    fn sc_state_parses_the_number_not_the_localized_word() {
        assert_eq!(
            sc_running_state("SERVICE_NAME: sshd\nTYPE : 10 WIN32_OWN_PROCESS\nSTATE : 4 RUNNING"),
            Some(4)
        );
        assert_eq!(
            sc_running_state("SERVICE_NAME: sshd\nTYPE : 10 WIN32_OWN_PROCESS\nSTATE : 1 STOPPED"),
            Some(1)
        );
        // A mock non-English run: the keyword after the number differs.
        assert_eq!(sc_running_state("SERVICE_NAME: sshd\nSTATE : 4  LÄUFT"), Some(4));
        assert_eq!(sc_running_state("no state line here"), None);
    }

    #[test]
    fn sc_start_type_parses_auto_demand() {
        assert_eq!(
            sc_start_type("SERVICE_NAME: sshd\nSTART_TYPE : 2 AUTO_START"),
            Some(SC_START_AUTO)
        );
        assert_eq!(sc_start_type("START_TYPE : 3 DEMAND_START"), Some(3));
        assert_eq!(sc_start_type("nothing"), None);
    }

    #[test]
    fn problems_and_readiness() {
        let ready = status(
            SshdState::Installed {
                running: true,
                auto_start: true,
            },
            true,
        );
        assert!(ready.ready());
        assert!(ready.problems().is_empty());

        let missing_everything = status(SshdState::NotInstalled, false);
        assert!(!missing_everything.ready());
        assert_eq!(missing_everything.problems().len(), 2);

        let stopped = status(
            SshdState::Installed {
                running: false,
                auto_start: true,
            },
            true,
        );
        assert_eq!(stopped.problems(), vec!["the sshd service is not running"]);

        let demand = status(
            SshdState::Installed {
                running: true,
                auto_start: false,
            },
            false,
        );
        assert_eq!(demand.problems().len(), 2);
    }

    #[test]
    fn report_marks_missing_and_all_set() {
        let ok = status(
            SshdState::Installed {
                running: true,
                auto_start: true,
            },
            true,
        );
        let report = status_report(&ok);
        assert!(report.contains("All set"));
        assert!(!report.contains("MISSING"));

        let bad = status(SshdState::NotInstalled, false);
        let report = status_report(&bad);
        assert!(report.contains("[MISSING] OpenSSH Server (sshd) feature"));
        assert!(report.contains("[MISSING] firewall rule"));
        assert!(report.contains("Needs attention: 2 problem(s)"));
    }

    #[test]
    fn planned_actions_cover_each_problem() {
        let s = status(SshdState::NotInstalled, false);
        let actions = planned_actions(&s);
        assert_eq!(actions.len(), 2);
        assert!(actions[0].contains("DISM"));
        assert!(actions[1].contains("firewall rule"));

        let s = status(
            SshdState::Installed {
                running: false,
                auto_start: false,
            },
            true,
        );
        let actions = planned_actions(&s);
        assert_eq!(actions.len(), 2);
        assert!(actions[0].contains("start automatically"));
        assert!(actions[1].contains("Start the sshd service"));

        let s = status(
            SshdState::Installed {
                running: true,
                auto_start: true,
            },
            true,
        );
        assert!(planned_actions(&s).is_empty());
    }

    #[test]
    fn confirm_text_numbers_the_plan() {
        let s = status(SshdState::NotInstalled, false);
        let text = confirm_text(&s);
        assert!(text.contains("1."));
        assert!(text.contains("2."));
        assert!(text.contains("UAC"));
    }
}