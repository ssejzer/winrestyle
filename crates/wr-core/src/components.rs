//! The component registry: the restylable surfaces the Phase 3 manager shows
//! as a checklist. Each [`Component`] knows how to read and flip its own on/off
//! state in a [`Config`], so the manager stays generic — it renders one row per
//! component and calls the trait, never special-casing taskbar vs. wallpaper.
//!
//! ## The trait's three verbs
//!
//! The roadmap names a component's lifecycle `install` / `uninstall` / `apply`:
//!
//! - [`Component::install`] turns the component on in a config (idempotent);
//! - [`Component::uninstall`] turns it off (idempotent);
//! - [`Registry::apply`] is the plural: given the set of ids the user checked,
//!   it produces the config that reflects exactly that selection, installing
//!   the checked components and uninstalling the rest. "Restyle Now" writes the
//!   result of `apply` to disk.
//!
//! All of this is pure config transformation — no Windows, no I/O — so it
//! unit-tests on the dev host and the manager's preview is exact.

use std::collections::BTreeSet;

use crate::config::Config;

/// One restylable surface (taskbar, wallpaper, autostart management). Object-safe
/// so the registry can hold a heterogeneous list.
pub trait Component {
    /// Stable id, used in the UI selection set and (for logging) nowhere
    /// user-facing. Kebab-case, e.g. `"taskbar"`.
    fn id(&self) -> &'static str;
    /// Display name for the checklist row.
    fn name(&self) -> &'static str;
    /// One-line description of what turning it on does.
    fn summary(&self) -> &'static str;
    /// Is the component currently on in `config`?
    fn is_installed(&self, config: &Config) -> bool;
    /// Turn it on. Idempotent.
    fn install(&self, config: &mut Config);
    /// Turn it off. Idempotent.
    fn uninstall(&self, config: &mut Config);
}

/// The taskbar surface (`[taskbar] enabled`).
pub struct TaskbarComponent;
impl Component for TaskbarComponent {
    fn id(&self) -> &'static str {
        "taskbar"
    }
    fn name(&self) -> &'static str {
        "Taskbar"
    }
    fn summary(&self) -> &'static str {
        "Rounded, translucent taskbar with Start, pinned apps, window buttons, tray, and clock."
    }
    fn is_installed(&self, config: &Config) -> bool {
        config.taskbar.enabled
    }
    fn install(&self, config: &mut Config) {
        config.taskbar.enabled = true;
    }
    fn uninstall(&self, config: &mut Config) {
        config.taskbar.enabled = false;
    }
}

/// The desktop wallpaper (`[wallpaper] enabled`).
pub struct WallpaperComponent;
impl Component for WallpaperComponent {
    fn id(&self) -> &'static str {
        "wallpaper"
    }
    fn name(&self) -> &'static str {
        "Wallpaper"
    }
    fn summary(&self) -> &'static str {
        "Paint a custom desktop color or image. Off leaves a neutral background."
    }
    fn is_installed(&self, config: &Config) -> bool {
        config.wallpaper.enabled
    }
    fn install(&self, config: &mut Config) {
        config.wallpaper.enabled = true;
    }
    fn uninstall(&self, config: &mut Config) {
        config.wallpaper.enabled = false;
    }
}

/// Logon-autostart management (`[autostart] enabled`). Turning it on lets the
/// shell run what explorer would at logon (ADR 0004); the per-entry opt-outs
/// are the manager's startup-programs list.
pub struct AutostartComponent;
impl Component for AutostartComponent {
    fn id(&self) -> &'static str {
        "autostart"
    }
    fn name(&self) -> &'static str {
        "Startup programs"
    }
    fn summary(&self) -> &'static str {
        "Run your logon startup apps (Run keys and Startup folders) in explorer's stead."
    }
    fn is_installed(&self, config: &Config) -> bool {
        config.autostart.enabled
    }
    fn install(&self, config: &mut Config) {
        config.autostart.enabled = true;
    }
    fn uninstall(&self, config: &mut Config) {
        config.autostart.enabled = false;
    }
}

/// The set of components the manager offers, in display order.
pub struct Registry {
    components: Vec<Box<dyn Component>>,
}

impl Registry {
    /// Every shippable component, in the order the checklist shows them.
    pub fn all() -> Self {
        Registry {
            components: vec![
                Box::new(TaskbarComponent),
                Box::new(WallpaperComponent),
                Box::new(AutostartComponent),
            ],
        }
    }

    /// The components, in display order.
    pub fn components(&self) -> &[Box<dyn Component>] {
        &self.components
    }

    /// The ids currently installed (on) in `config`.
    pub fn installed_ids(&self, config: &Config) -> BTreeSet<String> {
        self.components
            .iter()
            .filter(|c| c.is_installed(config))
            .map(|c| c.id().to_string())
            .collect()
    }

    /// Produce the config that reflects exactly `selected`: install every
    /// component whose id is in the set, uninstall the rest. Starts from
    /// `base` so unrelated settings (colors, pinned apps, the `disabled` list)
    /// are preserved. Ids in `selected` that name no known component are
    /// ignored. This is the roadmap's plural "apply".
    pub fn apply(&self, base: &Config, selected: &BTreeSet<String>) -> Config {
        let mut config = base.clone();
        for component in &self.components {
            if selected.contains(component.id()) {
                component.install(&mut config);
            } else {
                component.uninstall(&mut config);
            }
        }
        config
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(set: &[&str]) -> BTreeSet<String> {
        set.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_component_has_unique_id_and_nonempty_text() {
        let reg = Registry::all();
        let mut seen = BTreeSet::new();
        for c in reg.components() {
            assert!(seen.insert(c.id()), "duplicate id {}", c.id());
            assert!(!c.name().is_empty());
            assert!(!c.summary().is_empty());
        }
    }

    #[test]
    fn defaults_have_every_component_installed() {
        // Fresh Windows-parity defaults: taskbar, wallpaper, autostart all on.
        let reg = Registry::all();
        let installed = reg.installed_ids(&Config::default());
        assert_eq!(installed, ids(&["taskbar", "wallpaper", "autostart"]));
    }

    #[test]
    fn apply_installs_selected_and_uninstalls_the_rest() {
        let reg = Registry::all();
        let config = reg.apply(&Config::default(), &ids(&["taskbar"]));
        assert!(config.taskbar.enabled);
        assert!(!config.wallpaper.enabled);
        assert!(!config.autostart.enabled);
        assert_eq!(reg.installed_ids(&config), ids(&["taskbar"]));
    }

    #[test]
    fn apply_preserves_unrelated_settings() {
        let reg = Registry::all();
        let mut base = Config::default();
        base.taskbar.pinned = vec![std::path::PathBuf::from(r"C:\Windows\notepad.exe")];
        base.wallpaper.color = "#010203".parse().unwrap();
        base.autostart.disabled = vec!["hkcu-run:OneDrive".into()];

        let config = reg.apply(&base, &ids(&["wallpaper"]));
        // Selection changed, but the fiddly bits survive.
        assert_eq!(config.taskbar.pinned, base.taskbar.pinned);
        assert_eq!(config.wallpaper.color, base.wallpaper.color);
        assert_eq!(config.autostart.disabled, base.autostart.disabled);
    }

    #[test]
    fn install_uninstall_are_idempotent() {
        let c = TaskbarComponent;
        let mut config = Config::default();
        c.install(&mut config);
        c.install(&mut config);
        assert!(c.is_installed(&config));
        c.uninstall(&mut config);
        c.uninstall(&mut config);
        assert!(!c.is_installed(&config));
    }

    #[test]
    fn unknown_ids_in_selection_are_ignored() {
        let reg = Registry::all();
        let config = reg.apply(&Config::default(), &ids(&["taskbar", "does-not-exist"]));
        assert_eq!(reg.installed_ids(&config), ids(&["taskbar"]));
    }

    #[test]
    fn empty_selection_uninstalls_everything() {
        let reg = Registry::all();
        let config = reg.apply(&Config::default(), &BTreeSet::new());
        assert!(reg.installed_ids(&config).is_empty());
    }
}
