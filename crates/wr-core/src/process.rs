//! Killing stray WinRestyle processes by executable name.
//!
//! Both convergence and recovery lean on this: the watchdog sweeps stray
//! shells (and taskbars) at startup so a relaunch always converges to one of
//! each (ADR 0002), the shell sweeps stray taskbars before spawning its own
//! (ADR 0005), and every restore-explorer path sweeps our UI surfaces so an
//! emergency restore never leaves a WinRestyle window over the recovered
//! desktop.
//!
//! Only ever call this with WinRestyle's own executable names.

/// Kill every running process whose executable name matches `exe_name`
/// (case-insensitive, e.g. `"wr-taskbar.exe"`), except the calling process.
/// Failures are logged, never fatal — a sweep is always best-effort.
/// Returns the number of processes it managed to terminate.
#[cfg(windows)]
pub fn kill_all_named(exe_name: &str) -> usize {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    let target = exe_name.to_ascii_lowercase();
    let own_pid = std::process::id();

    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(s) => s,
        Err(e) => {
            log::warn!("{target} sweep skipped: process snapshot failed: {e}");
            return 0;
        }
    };

    let mut killed = 0;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
        let pid = entry.th32ProcessID;
        if name == target && pid != own_pid {
            log::warn!("killing stray {target} (pid {pid})");
            match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
                Ok(process) => unsafe {
                    match TerminateProcess(process, 1) {
                        Ok(()) => killed += 1,
                        Err(e) => log::error!("failed to kill stray pid {pid}: {e}"),
                    }
                    let _ = CloseHandle(process);
                },
                Err(e) => log::error!("failed to open stray pid {pid}: {e}"),
            }
        }
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    killed
}

#[cfg(not(windows))]
pub fn kill_all_named(_exe_name: &str) -> usize {
    0
}
