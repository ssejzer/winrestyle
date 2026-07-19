//! The logon-autostart *entry model*: stable ids and enumeration, shared by
//! the shell (which launches entries) and the Phase 3 manager (which lists
//! them and toggles them on/off).
//!
//! ## Why this lives in `wr-core`
//!
//! The shell matches a config's [`Autostart::disabled`](crate::config::Autostart)
//! list against the id of each entry it is about to launch; the manager writes
//! that same list from checkboxes. If the two sides ever formatted an id
//! differently — `hkcu-run:OneDrive` vs `HKCU\Run:OneDrive` — a checkbox in the
//! manager would silently fail to disable anything. The id constructors below
//! are the single source of truth for that wire format, unit-tested against the
//! exact strings the shell has always produced.
//!
//! Enumeration itself is Windows-only (it reads the registry and the Startup
//! folders); the id format and the [`AutostartEntry`] model are cross-platform
//! so the manager's view logic unit-tests on the Linux dev host.

/// Where an autostart entry comes from — drives the manager's grouping and the
/// human-readable "source" label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A per-user `Run` registry value (HKCU).
    HkcuRun,
    /// A machine-wide `Run` registry value (HKLM, launched read-only).
    HklmRun,
    /// A per-user `RunOnce` value (deleted as it runs, per the Windows contract).
    HkcuRunOnce,
    /// A per-user Startup-folder item (`%APPDATA%\…\Startup`).
    StartupUser,
    /// A machine-wide Startup-folder item (`%ProgramData%\…\Startup`).
    StartupCommon,
    /// A session helper the shell supplies itself (e.g. `rdpclip` in remote
    /// sessions).
    Session,
}

impl Source {
    /// The id prefix — the text before the `:` in an entry id. Changing any of
    /// these is a breaking change to the on-disk `disabled` list.
    pub fn prefix(self) -> &'static str {
        match self {
            Source::HkcuRun => "hkcu-run",
            Source::HklmRun => "hklm-run",
            Source::HkcuRunOnce => "hkcu-runonce",
            Source::StartupUser => "startup-user",
            Source::StartupCommon => "startup-common",
            Source::Session => "session",
        }
    }

    /// A short human label for the manager's UI ("what kind of thing is this").
    pub fn label(self) -> &'static str {
        match self {
            Source::HkcuRun => "Run (user)",
            Source::HklmRun => "Run (machine)",
            Source::HkcuRunOnce => "RunOnce (user)",
            Source::StartupUser => "Startup folder (user)",
            Source::StartupCommon => "Startup folder (machine)",
            Source::Session => "Session helper",
        }
    }
}

/// The id the shell logs and the config `disabled` list matches, e.g.
/// `hkcu-run:OneDrive` or `startup-user:Foo.lnk`. Case-insensitive on the
/// consuming side ([`Autostart::allows`](crate::config::Autostart::allows)).
pub fn entry_id(source: Source, name: &str) -> String {
    format!("{}:{}", source.prefix(), name)
}

/// One enumerated autostart entry, as the manager needs to display and toggle
/// it. No launch behavior here — that stays in the shell, which owns the
/// delete-before-run semantics and process spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartEntry {
    /// Stable id ([`entry_id`]); what goes in the config `disabled` list.
    pub id: String,
    /// The entry's own name (registry value name or file name).
    pub name: String,
    /// Where it came from.
    pub source: Source,
    /// A one-line detail for the UI: the command line or the file path.
    pub detail: String,
}

impl AutostartEntry {
    #[cfg_attr(not(windows), allow(dead_code))]
    fn new(source: Source, name: impl Into<String>, detail: impl Into<String>) -> Self {
        let name = name.into();
        AutostartEntry {
            id: entry_id(source, &name),
            name,
            source,
            detail: detail.into(),
        }
    }
}

/// Everything explorer (and therefore the shell) would launch at logon, as
/// display metadata, in roughly launch order. Enumeration only — nothing here
/// launches, deletes, or mutates. `Ok`-of-empty on a machine with nothing
/// registered; individual unreadable sources are skipped, never fatal.
///
/// This intentionally mirrors the shell's own enumeration
/// (`wr-shell::autostart::enumerate`); both build ids through [`entry_id`], so
/// a manager checkbox and the shell's launch filter always speak of the same
/// entry. HKLM `RunOnce` is deliberately omitted (the shell never runs it —
/// ADR 0004 — so there is nothing for the manager to toggle).
#[cfg(windows)]
pub fn enumerate() -> Vec<AutostartEntry> {
    imp::enumerate()
}

/// Non-Windows stub so the manager's view logic compiles and unit-tests on the
/// dev host. There are no logon-autostart entries off Windows.
#[cfg(not(windows))]
pub fn enumerate() -> Vec<AutostartEntry> {
    Vec::new()
}

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE};
    use winreg::types::FromRegValue;
    use winreg::{RegKey, HKEY};

    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};

    use super::{AutostartEntry, Source};

    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUNONCE_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";

    pub fn enumerate() -> Vec<AutostartEntry> {
        let mut entries = Vec::new();

        for (name, command) in reg_values(HKEY_CURRENT_USER, RUNONCE_SUBKEY) {
            entries.push(AutostartEntry::new(Source::HkcuRunOnce, name, command));
        }
        for (name, command) in reg_values(HKEY_LOCAL_MACHINE, RUN_SUBKEY) {
            entries.push(AutostartEntry::new(Source::HklmRun, name, command));
        }
        for (name, command) in reg_values(HKEY_CURRENT_USER, RUN_SUBKEY) {
            entries.push(AutostartEntry::new(Source::HkcuRun, name, command));
        }

        for (source, base) in [
            (Source::StartupCommon, std::env::var_os("ProgramData")),
            (Source::StartupUser, std::env::var_os("APPDATA")),
        ] {
            let Some(base) = base else { continue };
            let dir = PathBuf::from(base).join(r"Microsoft\Windows\Start Menu\Programs\Startup");
            let Ok(items) = std::fs::read_dir(&dir) else {
                continue;
            };
            for item in items.flatten() {
                let name = item.file_name().to_string_lossy().into_owned();
                if name.eq_ignore_ascii_case("desktop.ini") {
                    continue;
                }
                if item.file_type().is_ok_and(|t| t.is_file()) {
                    entries.push(AutostartEntry::new(
                        source,
                        name,
                        item.path().display().to_string(),
                    ));
                }
            }
        }

        if unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0 {
            entries.push(AutostartEntry::new(
                Source::Session,
                "rdpclip",
                "rdpclip.exe (clipboard redirection)",
            ));
        }

        entries
    }

    /// Non-empty string values of a registry key. A missing key is an empty
    /// list. Unlike the shell's launcher, this does not env-expand — the
    /// manager shows the command as written, which is what the user recognizes.
    fn reg_values(root: HKEY, subkey: &str) -> Vec<(String, String)> {
        let Ok(key) = RegKey::predef(root).open_subkey_with_flags(subkey, KEY_QUERY_VALUE) else {
            return Vec::new();
        };
        key.enum_values()
            .filter_map(|r| r.ok())
            .filter_map(|(name, value)| {
                let command = String::from_reg_value(&value).ok()?;
                let command = command.trim().to_string();
                (!name.is_empty() && !command.is_empty()).then_some((name, command))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_match_the_shells_historical_wire_format() {
        // These exact strings are what wr-shell::autostart has always logged
        // and matched against; the shell's `disabled`-list contract depends on
        // them not changing.
        assert_eq!(entry_id(Source::HkcuRun, "OneDrive"), "hkcu-run:OneDrive");
        assert_eq!(entry_id(Source::HklmRun, "Foo"), "hklm-run:Foo");
        assert_eq!(
            entry_id(Source::HkcuRunOnce, "!Setup"),
            "hkcu-runonce:!Setup"
        );
        assert_eq!(
            entry_id(Source::StartupUser, "Thing.lnk"),
            "startup-user:Thing.lnk"
        );
        assert_eq!(
            entry_id(Source::StartupCommon, "Corp.lnk"),
            "startup-common:Corp.lnk"
        );
        assert_eq!(entry_id(Source::Session, "rdpclip"), "session:rdpclip");
    }

    #[test]
    fn entry_id_is_built_from_source_and_name() {
        let e = AutostartEntry::new(Source::HkcuRun, "App", "C:\\app.exe /bg");
        assert_eq!(e.id, "hkcu-run:App");
        assert_eq!(e.name, "App");
        assert_eq!(e.source, Source::HkcuRun);
        assert_eq!(e.detail, "C:\\app.exe /bg");
    }

    #[test]
    fn every_source_has_distinct_prefix_and_a_label() {
        let all = [
            Source::HkcuRun,
            Source::HklmRun,
            Source::HkcuRunOnce,
            Source::StartupUser,
            Source::StartupCommon,
            Source::Session,
        ];
        let mut prefixes: Vec<&str> = all.iter().map(|s| s.prefix()).collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        assert_eq!(prefixes.len(), all.len(), "prefixes must be unique");
        assert!(all.iter().all(|s| !s.label().is_empty()));
    }

    #[test]
    fn enumerate_off_windows_is_empty() {
        // The cross-platform stub keeps the manager compiling on the dev host.
        #[cfg(not(windows))]
        assert!(enumerate().is_empty());
    }
}
