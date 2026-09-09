//! The platform layer: every place autofork touches the operating system
//! directly, behind one cross-platform surface.
//!
//! autofork runs on macOS, Linux and native Windows. The semantics above
//! this module are identical everywhere; what differs is *how* the OS is
//! asked — which directory is "home", whether a pid is alive, how a child
//! is detached from the process that spawned it, which shell runs a hook
//! command, how a lock file is locked. Each of those is one function here
//! with a Unix body and a Windows body, so no other module carries a
//! `cfg(windows)`.
//!
//! What is deliberately *not* here: the daemon's IPC endpoint. A Unix domain
//! socket on Unix and a named pipe on Windows are different enough (sync
//! `std` on the client, tokio on the daemon) that each side keeps its own
//! small adapter; this module only names the endpoint
//! ([`pipe_name`](crate::config::Paths::pipe_name)).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The executable suffix of this platform (`.exe` on Windows, empty
/// elsewhere) — for naming sibling binaries next to the current one.
pub const EXE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

/// The user's home directory.
///
/// Unix: `$HOME`. Windows: `%USERPROFILE%` (then `%HOMEDRIVE%%HOMEPATH%`),
/// deliberately *before* `HOME` — Git Bash, which is what runs the plugin's
/// hook shims on Windows, exports a POSIX-shaped `HOME` (`/c/Users/x`) that
/// no Win32 file API can open.
pub fn home_dir() -> Option<PathBuf> {
    let non_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var_os("USERPROFILE").and_then(non_empty) {
            return Some(PathBuf::from(p));
        }
        if let (Some(d), Some(p)) = (
            std::env::var_os("HOMEDRIVE").and_then(non_empty),
            std::env::var_os("HOMEPATH").and_then(non_empty),
        ) {
            let mut out = d;
            out.push(p);
            return Some(PathBuf::from(out));
        }
    }
    std::env::var_os("HOME")
        .and_then(non_empty)
        .map(PathBuf::from)
}

/// A directory that always exists, for a process whose real working
/// directory is gone (a session launched from a since-deleted temp dir).
pub fn root_fallback_dir() -> PathBuf {
    #[cfg(windows)]
    {
        home_dir().unwrap_or_else(std::env::temp_dir)
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/")
    }
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// The pid of this process's parent.
pub fn parent_pid() -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::process::parent_id()
    }
    #[cfg(windows)]
    {
        win::parent_of(std::process::id()).unwrap_or(0)
    }
}

/// Whether a pid names a live process. On Unix, `EPERM` counts (it exists,
/// we just don't own it); on Windows, an access-denied open counts the same
/// way, and a process whose handle is still open but whose exit code is set
/// is dead.
pub fn pid_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe {
            if libc::kill(pid as libc::pid_t, 0) == 0 {
                return true;
            }
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        win::pid_exists(pid)
    }
}

/// A process's parent pid.
pub fn parent_of(pid: u32) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        mac::bsdinfo(pid).map(|info| info.pbi_ppid)
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat field 4, after the parenthesized comm.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(1)?.parse::<u32>().ok()
    }
    #[cfg(windows)]
    {
        win::parent_of(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

/// The executable path of a running process, when the platform exposes it.
pub fn exe_path(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        mac::pidpath(pid)
    }
    #[cfg(windows)]
    {
        win::exe_path(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

/// A process's start time as an opaque token. Only ever compared against
/// another token for the same pid taken on the same machine.
///
/// macOS: absolute wall-clock microseconds. Windows: the kernel's creation
/// `FILETIME` (absolute, 100ns since 1601). Both survive a reboot as an
/// identity. Linux: `starttime` in clock ticks since boot, so it is only
/// unique within a boot — the harness's `bin` is the belt there.
pub fn start_token(pid: u32) -> Option<i64> {
    #[cfg(target_os = "macos")]
    {
        let info = mac::bsdinfo(pid)?;
        Some(info.pbi_start_tvsec as i64 * 1_000_000 + info.pbi_start_tvusec as i64)
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat field 22, counting from 1. The comm field (2) can
        // contain spaces and parentheses, so split after the LAST ')'.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(19)?.parse::<i64>().ok()
    }
    #[cfg(windows)]
    {
        win::start_token(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

/// Whether an executable name is a shell (or a shell-shaped exec wrapper) —
/// a process that stands between us and a client rather than being it.
/// Case-insensitive and `.exe`-blind, so `bash.exe` under Git for Windows
/// reads the same as `/bin/bash`.
pub fn is_shell_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    matches!(
        stem,
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "ksh"
            | "fish"
            | "env"
            | "login"
            | "script"
            | "cmd"
            | "powershell"
            | "pwsh"
            | "conhost"
    )
}

/// Detach `cmd` from the process that spawns it, so it outlives a hook that
/// exits (or is killed) moments later, and a closed terminal cannot take it
/// down.
///
/// Unix: a new session (`setsid`) — closing the parent's terminal window
/// SIGHUPs the process group, and a fork's WORK should survive the session
/// closing even when its report cannot. Windows: a new process group with no
/// inherited console — the same two properties (no console close event, no
/// Ctrl+C from the parent's group).
pub fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(win::CREATE_NEW_PROCESS_GROUP | win::DETACHED_PROCESS);
    }
}

/// The program and leading arguments that run a shell command string:
/// `/bin/sh -c` on Unix; on Windows the Git for Windows `bash.exe` (the
/// shell Claude Code itself runs hook commands through there), and
/// `cmd.exe /C` as the last resort when no bash can be found.
pub fn shell() -> (PathBuf, &'static [&'static str]) {
    #[cfg(unix)]
    {
        (PathBuf::from("/bin/sh"), &["-c"])
    }
    #[cfg(windows)]
    {
        match win::find_bash() {
            Some(bash) => (bash, &["-c"]),
            None => (PathBuf::from("cmd.exe"), &["/C"]),
        }
    }
}

/// Whether [`shell`] resolved a real POSIX shell (for `doctor`).
pub fn shell_is_posix() -> bool {
    shell().1 == ["-c"]
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Acquire (and immediately hold) a non-blocking exclusive lock on `path`,
/// creating it as needed; `None` when another process holds it. Dropping
/// the file releases the lock — and so does the holder dying, which is what
/// makes "acquirable" mean "the daemon is dead".
pub fn try_lock_file(path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        (rc == 0).then_some(file)
    }
    #[cfg(windows)]
    {
        win::try_lock(&file).then_some(file)
    }
}

/// Replace `path`'s target with a link to (or, where links need privileges
/// the process may not have, a copy of) `target`. Best effort.
pub fn link_or_copy(target: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, path)
    }
    #[cfg(windows)]
    {
        // Symlinks need Developer Mode or elevation on Windows; a copy is
        // the version of the same file that always works.
        std::os::windows::fs::symlink_file(target, path)
            .or_else(|_| std::fs::copy(target, path).map(|_| ()))
    }
}

/// A 64-bit FNV-1a hash — for deriving short, stable names from paths.
pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;

    /// One `proc_pidinfo(PROC_PIDTBSDINFO)` read — libproc, part of libSystem.
    pub fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (n == size).then_some(info)
    }

    /// `proc_pidpath` from libproc (part of libSystem — no extra linking).
    pub fn pidpath(pid: u32) -> Option<PathBuf> {
        extern "C" {
            fn proc_pidpath(
                pid: libc::c_int,
                buffer: *mut libc::c_void,
                buffersize: u32,
            ) -> libc::c_int;
        }
        let mut buf = [0u8; 4096];
        let n = unsafe {
            proc_pidpath(
                pid as libc::c_int,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as u32,
            )
        };
        if n <= 0 {
            return None;
        }
        let path = std::str::from_utf8(&buf[..n as usize]).ok()?;
        Some(PathBuf::from(path))
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
        STILL_ACTIVE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    pub use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    /// An open process handle, closed on drop.
    struct Proc(HANDLE);

    impl Drop for Proc {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn open(pid: u32) -> Result<Proc, u32> {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            Err(unsafe { GetLastError() })
        } else {
            Ok(Proc(h))
        }
    }

    pub fn pid_exists(pid: u32) -> bool {
        match open(pid) {
            Ok(p) => {
                // A handle can still be opened on a process that has exited
                // but not been reaped by everyone holding it; the exit code
                // tells those apart.
                let mut code = 0u32;
                unsafe { GetExitCodeProcess(p.0, &mut code) != 0 && code == STILL_ACTIVE as u32 }
            }
            Err(ERROR_ACCESS_DENIED) => true,
            Err(_) => false,
        }
    }

    pub fn start_token(pid: u32) -> Option<i64> {
        let p = open(pid).ok()?;
        let mut created: FILETIME = unsafe { std::mem::zeroed() };
        let mut exited: FILETIME = unsafe { std::mem::zeroed() };
        let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
        let mut user: FILETIME = unsafe { std::mem::zeroed() };
        let ok =
            unsafe { GetProcessTimes(p.0, &mut created, &mut exited, &mut kernel, &mut user) != 0 };
        ok.then(|| ((created.dwHighDateTime as i64) << 32) | created.dwLowDateTime as i64)
    }

    pub fn exe_path(pid: u32) -> Option<PathBuf> {
        let p = open(pid).ok()?;
        let mut buf = vec![0u16; 32 * 1024];
        let mut len = buf.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(p.0, 0, buf.as_mut_ptr(), &mut len) != 0 };
        ok.then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }

    pub fn parent_of(pid: u32) -> Option<u32> {
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let snap = Proc(snap);
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if unsafe { Process32FirstW(snap.0, &mut entry) } == 0 {
            return None;
        }
        loop {
            if entry.th32ProcessID == pid {
                return Some(entry.th32ParentProcessID);
            }
            if unsafe { Process32NextW(snap.0, &mut entry) } == 0 {
                return None;
            }
        }
    }

    pub fn try_lock(file: &std::fs::File) -> bool {
        use std::os::windows::io::AsRawHandle;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        unsafe {
            LockFileEx(
                file.as_raw_handle() as HANDLE,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                u32::MAX,
                u32::MAX,
                &mut ov,
            ) != 0
        }
    }

    /// Git for Windows' `bash.exe`. `CLAUDE_CODE_GIT_BASH_PATH` (the setting
    /// Claude Code itself honors) wins; then the `git.exe` on PATH, whose
    /// install tree holds `bin\bash.exe`; then the standard install
    /// locations. `System32\bash.exe` is never considered — that one is the
    /// WSL launcher, and a WSL distro is not this machine.
    pub fn find_bash() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH") {
            let p = PathBuf::from(p);
            if p.is_file() {
                return Some(p);
            }
        }
        if let Some(git) = which("git.exe") {
            // Git\cmd\git.exe, Git\bin\git.exe or Git\mingw64\bin\git.exe:
            // walk up until a bin\bash.exe sibling appears.
            let mut dir = git.parent();
            for _ in 0..3 {
                let Some(d) = dir else { break };
                let candidate = d.join("bin").join("bash.exe");
                if candidate.is_file() {
                    return Some(candidate);
                }
                dir = d.parent();
            }
        }
        let roots = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
            .iter()
            .filter_map(|v| std::env::var_os(v))
            .map(|v| PathBuf::from(v).join("Git"))
            .chain(
                std::env::var_os("LOCALAPPDATA")
                    .map(|v| PathBuf::from(v).join("Programs").join("Git")),
            );
        for root in roots {
            let candidate = root.join("bin").join("bash.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    /// A PATH lookup for one exact file name.
    fn which(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }

    #[allow(dead_code)]
    fn _assert_path_types(_: &Path) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_resolved() {
        assert!(home_dir().is_some(), "the test runner always has a home");
    }

    #[test]
    fn own_process_is_alive_with_a_stable_token_and_a_parent() {
        let me = std::process::id();
        assert!(pid_exists(me));
        assert_eq!(start_token(me), start_token(me));
        assert_eq!(parent_of(me), Some(parent_pid()));
        assert!(parent_pid() > 0);
        let exe = exe_path(me).expect("own exe path");
        assert!(exe.is_absolute());
    }

    #[test]
    fn shell_names_are_recognized_on_every_platform() {
        assert!(is_shell_name("sh"));
        assert!(is_shell_name("bash.exe"));
        assert!(is_shell_name("BASH.EXE"));
        assert!(is_shell_name("cmd.exe"));
        assert!(!is_shell_name("claude"));
        assert!(!is_shell_name("claude.exe"));
        assert!(!is_shell_name("node.exe"));
    }

    #[test]
    fn lock_is_exclusive_within_a_process_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run").join("x.lock");
        let held = try_lock_file(&path).expect("first lock");
        assert!(try_lock_file(&path).is_none(), "second lock must fail");
        drop(held);
        assert!(try_lock_file(&path).is_some(), "released on drop");
    }

    #[test]
    fn shell_runs_a_command() {
        let (prog, args) = shell();
        let out = Command::new(&prog)
            .args(args)
            .arg("echo autofork")
            .output()
            .expect("the platform shell runs");
        assert!(String::from_utf8_lossy(&out.stdout).contains("autofork"));
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a64("a"), fnv1a64("b"));
    }
}
