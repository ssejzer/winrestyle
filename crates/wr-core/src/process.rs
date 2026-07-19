//! Killing processes by executable name — WinRestyle's own strays, and (for
//! live activation) the outgoing desktop and the session tree it spawned.
//!
//! Both convergence and recovery lean on the name-based kill: the watchdog
//! sweeps stray shells (and taskbars) at startup so a relaunch always
//! converges to one of each (ADR 0002), the shell sweeps stray taskbars before
//! spawning its own (ADR 0005), and every restore-explorer path sweeps our UI
//! surfaces so an emergency restore never leaves a WinRestyle window over the
//! recovered desktop.
//!
//! Call the name kills only with WinRestyle's own executable names, with one
//! documented exception used solely by `manager::activate_now` (ADR 0008):
//! [`kill_tree_named`]`("explorer.exe")` stops the outgoing desktop **and the
//! descendant tree it spawned** — the apps the user launched from the old
//! shell — because live activation stands in for a logout, which terminates
//! them all. Two guards make that safe: it never kills the calling process or
//! the ancestor chain that launched it (so the terminal or manager window
//! driving the swap survives), and — unlike a real logout — the kill is
//! forceful (no `WM_QUERYENDSESSION` save prompt), so both entry points
//! confirm first.

use std::collections::HashSet;

/// Transitive closure of the descendants of `seeds` in a `(pid, parent-pid)`
/// forest. The seeds themselves are not included. Cycle-safe (PID reuse can
/// forge a parent loop) via a growing visited set. Pure, so the tree walk
/// unit-tests on the dev host; the Win32 snapshot is the only platform part.
pub fn descendant_pids(procs: &[(u32, u32)], seeds: &[u32]) -> Vec<u32> {
    let seed_set: HashSet<u32> = seeds.iter().copied().collect();
    let mut found: HashSet<u32> = HashSet::new();
    let mut frontier: HashSet<u32> = seed_set.clone();
    while !frontier.is_empty() {
        let mut next: HashSet<u32> = HashSet::new();
        for &(pid, ppid) in procs {
            if pid == ppid || seed_set.contains(&pid) || found.contains(&pid) {
                continue;
            }
            if frontier.contains(&ppid) {
                found.insert(pid);
                next.insert(pid);
            }
        }
        frontier = next;
    }
    found.into_iter().collect()
}

/// The ancestor chain of `start` (its parent, grandparent, … up to the root)
/// in a `(pid, parent-pid)` forest. `start` itself is not included; a
/// parent-pid of 0 ends the walk. Cycle-safe.
pub fn ancestor_pids(procs: &[(u32, u32)], start: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut seen: HashSet<u32> = HashSet::from([start]);
    let mut cur = start;
    while let Some(&(_, ppid)) = procs.iter().find(|(pid, _)| *pid == cur) {
        if ppid == 0 || !seen.insert(ppid) {
            break;
        }
        chain.push(ppid);
        cur = ppid;
    }
    chain
}

/// `(pid, parent pid, lowercased exe name)` for every process. Empty on
/// snapshot failure (logged) — every caller is best-effort.
#[cfg(windows)]
fn snapshot_procs() -> Vec<(u32, u32, String)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(s) => s,
        Err(e) => {
            log::warn!("process snapshot failed: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
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
        out.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    out
}

/// Terminate one process by pid. Returns whether it was killed; failures log.
#[cfg(windows)]
fn kill_pid(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
        Ok(process) => unsafe {
            let ok = match TerminateProcess(process, 1) {
                Ok(()) => true,
                Err(e) => {
                    log::error!("failed to kill pid {pid}: {e}");
                    false
                }
            };
            let _ = CloseHandle(process);
            ok
        },
        Err(e) => {
            log::error!("failed to open pid {pid}: {e}");
            false
        }
    }
}

/// The pids of every running process whose executable name matches `exe_name`
/// (case-insensitive), excluding the calling process.
#[cfg(windows)]
fn pids_named(exe_name: &str) -> Vec<u32> {
    let target = exe_name.to_ascii_lowercase();
    let own = std::process::id();
    snapshot_procs()
        .into_iter()
        .filter(|(pid, _, name)| *name == target && *pid != own)
        .map(|(pid, _, _)| pid)
        .collect()
}

/// Kill every running process whose executable name matches `exe_name`
/// (case-insensitive, e.g. `"wr-taskbar.exe"`), except the calling process.
/// Failures are logged, never fatal — a sweep is always best-effort.
/// Returns the number of processes it managed to terminate.
#[cfg(windows)]
pub fn kill_all_named(exe_name: &str) -> usize {
    let target = exe_name.to_ascii_lowercase();
    let mut killed = 0;
    for pid in pids_named(exe_name) {
        log::warn!("killing stray {target} (pid {pid})");
        if kill_pid(pid) {
            killed += 1;
        }
    }
    killed
}

/// Whether any process with this executable name is running (excluding the
/// calling process).
#[cfg(windows)]
pub fn any_named(exe_name: &str) -> bool {
    !pids_named(exe_name).is_empty()
}

/// Kill every process named `exe_name` **and the descendant tree each
/// spawned**, sparing this process and the ancestor chain that launched it.
/// For live activation (ADR 0008): the outgoing desktop plus the apps it
/// launched — what a logout would end — while never cutting the branch we sit
/// on (the terminal or manager window driving the swap, and, in a console
/// run, the parent process waiting on us). Best-effort; returns the count
/// terminated. See the module docs for why explorer is the one non-WinRestyle
/// name this is called with.
#[cfg(windows)]
pub fn kill_tree_named(exe_name: &str) -> usize {
    let procs = snapshot_procs();
    let target = exe_name.to_ascii_lowercase();
    let own = std::process::id();
    let seeds: Vec<u32> = procs
        .iter()
        .filter(|(pid, _, name)| *name == target && *pid != own)
        .map(|(pid, _, _)| *pid)
        .collect();
    if seeds.is_empty() {
        return 0;
    }
    let pairs: Vec<(u32, u32)> = procs.iter().map(|(p, pp, _)| (*p, *pp)).collect();
    // Never cut the branch we sit on: this process and everything above it
    // (which, when launched from a terminal that is itself a child of the
    // desktop, keeps that terminal alive even as the desktop dies).
    let mut protected: HashSet<u32> = ancestor_pids(&pairs, own).into_iter().collect();
    protected.insert(own);
    let victims: Vec<u32> = descendant_pids(&pairs, &seeds)
        .into_iter()
        .filter(|p| !protected.contains(p))
        .collect();
    let mut killed = 0;
    // Descendants first, then the desktop processes themselves. Seeds are
    // killed even if in the ancestor chain — stopping the desktop is the point.
    for pid in victims.iter().chain(seeds.iter()) {
        if *pid == own {
            continue;
        }
        log::warn!("live activate: killing pid {pid} (outgoing desktop tree)");
        if kill_pid(*pid) {
            killed += 1;
        }
    }
    killed
}

#[cfg(not(windows))]
pub fn kill_all_named(_exe_name: &str) -> usize {
    0
}

#[cfg(not(windows))]
pub fn any_named(_exe_name: &str) -> bool {
    false
}

#[cfg(not(windows))]
pub fn kill_tree_named(_exe_name: &str) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendants_are_transitive_and_exclude_seeds() {
        // 1 → 2 → 4, 1 → 3; 5 is unrelated (parent 9 not in the forest).
        let procs = [(1, 0), (2, 1), (3, 1), (4, 2), (5, 9)];
        let mut d = descendant_pids(&procs, &[1]);
        d.sort();
        assert_eq!(d, vec![2, 3, 4]);
        // Seeds are never in their own descendant set.
        let mut sub = descendant_pids(&procs, &[2]);
        sub.sort();
        assert_eq!(sub, vec![4]);
        // A leaf seed has no descendants.
        assert!(descendant_pids(&procs, &[5]).is_empty());
    }

    #[test]
    fn descendants_survive_a_pid_reuse_cycle() {
        // A forged loop 1 ↔ 2 must terminate, not spin.
        let procs = [(1, 2), (2, 1)];
        let _ = descendant_pids(&procs, &[1]);
    }

    #[test]
    fn ancestors_walk_to_the_root() {
        let procs = [(1, 0), (2, 1), (4, 2)];
        assert_eq!(ancestor_pids(&procs, 4), vec![2, 1]);
        assert!(ancestor_pids(&procs, 1).is_empty()); // parent 0 = root
    }

    #[test]
    fn ancestors_survive_a_cycle() {
        let procs = [(1, 2), (2, 1)];
        let _ = ancestor_pids(&procs, 1);
    }

    #[test]
    fn tree_kill_spares_the_invoking_branch() {
        // desktop(10) → terminal(20) → installer(30, "self"); desktop → app(40).
        // Activating from the terminal must kill the desktop and the sibling
        // app, but never the terminal (ancestor) or the installer (self).
        let procs = [(10, 1), (20, 10), (30, 20), (40, 10)];
        let seeds = [10u32];
        let mut protected: HashSet<u32> = ancestor_pids(&procs, 30).into_iter().collect();
        protected.insert(30);
        let mut victims: Vec<u32> = descendant_pids(&procs, &seeds)
            .into_iter()
            .filter(|p| !protected.contains(p))
            .collect();
        victims.sort();
        assert_eq!(victims, vec![40]); // 20 (ancestor) and 30 (self) spared
        assert!(protected.contains(&20) && protected.contains(&10));
    }
}
