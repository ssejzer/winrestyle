//! Start-menu application discovery (ADR 0007): walk the Start Menu
//! `Programs` folders and produce the flat, sorted list the menu shows.
//! Pure `std::fs`, so it unit-tests on the Linux dev host with temp dirs.

use std::collections::HashSet;
use std::path::PathBuf;

/// Junction/symlink loops are skipped outright (see `scan`), so this cap is
/// only a backstop against pathologically deep real trees.
const MAX_DEPTH: usize = 8;

/// One launchable Start Menu entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    /// Display name: the shortcut's file stem.
    pub name: String,
    /// Full path to the `.lnk`/`.url`, opened like a double-click.
    pub path: PathBuf,
}

/// The Programs roots to merge, user first — user entries shadow machine
/// entries with the same relative path, mirroring explorer's merge. Reading
/// the machine folder is read-only; the HKLM invariant is about writes.
pub fn roots() -> Vec<PathBuf> {
    ["APPDATA", "ProgramData"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|base| {
            PathBuf::from(base)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
        })
        .collect()
}

/// Walk `roots` (earlier roots win on duplicate relative paths) for
/// `.lnk`/`.url` shortcuts, sorted by name case-insensitively. Unreadable
/// directories are skipped — a menu with holes beats no menu.
pub fn scan(roots: &[PathBuf]) -> Vec<AppEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                // read_dir file types don't follow links, so junction/symlink
                // loops never recurse.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    if depth < MAX_DEPTH {
                        stack.push((path, depth + 1));
                    }
                    continue;
                }
                let is_shortcut = path.extension().is_some_and(|e| {
                    e.eq_ignore_ascii_case("lnk") || e.eq_ignore_ascii_case("url")
                });
                if !is_shortcut {
                    continue;
                }
                let name = match path.file_stem() {
                    Some(s) if !s.is_empty() => s.to_string_lossy().into_owned(),
                    _ => continue,
                };
                let rel = path
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if !seen.insert(rel) {
                    continue; // shadowed by an earlier root
                }
                out.push(AppEntry { name, path });
            }
        }
    }
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// Indices into `apps` whose names match `filter`: case-insensitive
/// substring, empty matches everything. Indices (not clones) so the menu's
/// filtered view borrows the scanned list.
pub fn filter_indices(apps: &[AppEntry], filter: &str) -> Vec<usize> {
    let needle = filter.to_lowercase();
    apps.iter()
        .enumerate()
        .filter(|(_, a)| needle.is_empty() || a.name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A scratch root unique to this test, with the given files created
    /// (paths relative to the root; parents are created).
    fn root(test: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wr-apps-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in files {
            let path = dir.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"").unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(apps: &[AppEntry]) -> Vec<&str> {
        apps.iter().map(|a| a.name.as_str()).collect()
    }

    #[test]
    fn scans_recursively_and_sorts_case_insensitively() {
        let r = root(
            "sort",
            &["zebra.lnk", "Accessories/alpha.lnk", "beta.url", "Mid.lnk"],
        );
        let apps = scan(&[r]);
        assert_eq!(names(&apps), vec!["alpha", "beta", "Mid", "zebra"]);
    }

    #[test]
    fn non_shortcuts_are_ignored() {
        let r = root(
            "ext",
            &[
                "desktop.ini",
                "readme.txt",
                "app.lnk",
                "tool.LNK",
                "site.URL",
            ],
        );
        let apps = scan(&[r]);
        assert_eq!(names(&apps), vec!["app", "site", "tool"]);
    }

    #[test]
    fn user_root_shadows_machine_on_same_relative_path() {
        let user = root("shadow-user", &["Sub/App.lnk", "UserOnly.lnk"]);
        let machine = root("shadow-machine", &["Sub/App.lnk", "MachineOnly.lnk"]);
        let apps = scan(&[user.clone(), machine]);
        assert_eq!(names(&apps), vec!["App", "MachineOnly", "UserOnly"]);
        // The surviving duplicate is the user's copy.
        let app = apps.iter().find(|a| a.name == "App").unwrap();
        assert!(app.path.starts_with(&user));
    }

    #[test]
    fn shadowing_is_case_insensitive_on_the_relative_path() {
        let user = root("case-user", &["Tools/APP.lnk"]);
        let machine = root("case-machine", &["tools/app.lnk"]);
        let apps = scan(&[user, machine]);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "APP");
    }

    #[test]
    fn same_name_in_different_folders_is_not_a_duplicate() {
        let r = root("twins", &["A/Setup.lnk", "B/Setup.lnk"]);
        let apps = scan(&[r]);
        assert_eq!(apps.len(), 2);
    }

    #[test]
    fn missing_root_is_empty_not_an_error() {
        let ghost = std::env::temp_dir().join("wr-apps-does-not-exist");
        assert!(scan(&[ghost]).is_empty());
        assert!(scan(&[]).is_empty());
    }

    #[test]
    fn filter_is_case_insensitive_substring_and_empty_matches_all() {
        let r = root("filter", &["Notepad.lnk", "Paint.lnk", "Note Taker.lnk"]);
        let apps = scan(&[r]);
        assert_eq!(names(&apps), vec!["Note Taker", "Notepad", "Paint"]);
        assert_eq!(filter_indices(&apps, ""), vec![0, 1, 2]);
        assert_eq!(filter_indices(&apps, "note"), vec![0, 1]);
        assert_eq!(filter_indices(&apps, "PAINT"), vec![2]);
        assert!(filter_indices(&apps, "nomatch").is_empty());
    }

    #[test]
    fn roots_are_built_from_env_vars() {
        // Only shape-checking the suffix; the env vars themselves are the
        // OS's business (and unset on the Linux dev host).
        std::env::set_var("APPDATA", std::env::temp_dir().join("wr-apps-appdata"));
        let roots = roots();
        assert!(!roots.is_empty());
        assert!(roots[0].ends_with(Path::new("Start Menu").join("Programs")));
    }
}
