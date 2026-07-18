//! User configuration: `%APPDATA%\WinRestyle\config.toml`.
//!
//! Loading must never take the shell down. The rules, in order of paranoia:
//!
//! - **Missing file** → defaults. A fresh install has no config.
//! - **Invalid file at startup** → defaults, with the parse error logged. The
//!   desktop always comes up.
//! - **Invalid file on reload** → keep the config we already had. A typo saved
//!   mid-edit must not yank settings out from under a running shell.
//!
//! Unknown keys are ignored (serde's default), so configs written by a newer
//! installer still load on an older shell.
//!
//! [`ConfigStore`] is the shared handle: load once at startup, `get()` from
//! wherever settings are consumed, `reload()` when [`wr_ipc::ToShell::ReloadConfig`]
//! arrives.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// `config.toml`, deserialized. Every field has a default so any subset of the
/// file (including none of it) is valid.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub wallpaper: Wallpaper,
    pub autostart: Autostart,
    pub taskbar: Taskbar,
}

/// `[wallpaper]` — what the shell paints behind everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Wallpaper {
    /// Solid fill color, `"#rrggbb"`.
    pub color: Color,
    /// Optional image; when set it wins over `color` (which still shows while
    /// the image loads or if it fails to).
    pub image: Option<PathBuf>,
}

impl Default for Wallpaper {
    fn default() -> Self {
        Wallpaper {
            color: Color {
                r: 0x1a,
                g: 0x1a,
                b: 0x2e,
            },
            image: None,
        }
    }
}

/// `[autostart]` — what the shell launches at logon in explorer's stead
/// (ADR 0004). Defaults mirror Windows: everything runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Autostart {
    /// Master switch. `false` launches nothing at all.
    pub enabled: bool,
    /// Entry ids to skip, e.g. `"hkcu-run:OneDrive"`,
    /// `"startup-common:foo.lnk"`, `"session:rdpclip"`. Case-insensitive.
    /// The shell logs every entry's id, so the list can be built from logs
    /// until the Phase 3 manager UI exists.
    pub disabled: Vec<String>,
}

impl Default for Autostart {
    fn default() -> Self {
        Autostart {
            enabled: true,
            disabled: Vec::new(),
        }
    }
}

impl Autostart {
    /// Should this entry launch? Case-insensitive on the id.
    pub fn allows(&self, id: &str) -> bool {
        self.enabled && !self.disabled.iter().any(|d| d.eq_ignore_ascii_case(id))
    }
}

/// `[taskbar]` — the Phase 2 taskbar surface. All lengths are physical pixels
/// at 96 DPI; the taskbar scales them by the monitor's actual DPI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Taskbar {
    /// Master switch. `false` means the shell does not spawn the taskbar.
    pub enabled: bool,
    /// Bar height.
    pub height: u32,
    /// Bar fill color.
    pub color: Color,
    /// Bar opacity, `0` (invisible) to `255` (fully opaque).
    pub alpha: u8,
    /// Rounded-corner radius. `0` = square.
    pub corner_radius: u32,
    /// Gap between the bar and the screen's bottom/side edges. `0` = docked
    /// edge to edge.
    pub margin: u32,
}

impl Default for Taskbar {
    fn default() -> Self {
        Taskbar {
            enabled: true,
            height: 48,
            color: Color {
                r: 0x10,
                g: 0x10,
                b: 0x1a,
            },
            alpha: 0xe0,
            corner_radius: 12,
            margin: 8,
        }
    }
}

/// An sRGB color, written in TOML as `"#rrggbb"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl std::str::FromStr for Color {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix('#')
            .filter(|h| h.len() == 6)
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| format!("invalid color {s:?} (expected \"#rrggbb\")"))?;
        Ok(Color {
            r: (hex >> 16) as u8,
            g: (hex >> 8) as u8,
            b: hex as u8,
        })
    }
}

impl TryFrom<String> for Color {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Color> for String {
    fn from(c: Color) -> String {
        c.to_string()
    }
}

impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// `%APPDATA%\WinRestyle\config.toml`. `None` when `APPDATA` is unset (never
/// the case in a real logon session).
pub fn default_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("WinRestyle")
            .join("config.toml"),
    )
}

/// Read and parse the file. `Ok(None)` means "no file" — a normal state, not
/// an error. `Err` means the file exists but is unreadable or malformed.
fn read(path: &Path) -> anyhow::Result<Option<Config>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context("reading config")),
    };
    let config = toml::from_str(&text).map_err(|e| anyhow::anyhow!("parsing config: {e}"))?;
    Ok(Some(config))
}

/// The current config plus where it came from. Shared (e.g. `Arc<ConfigStore>`)
/// between whatever consumes settings and the IPC thread that reloads them.
pub struct ConfigStore {
    path: Option<PathBuf>,
    current: RwLock<Config>,
}

impl ConfigStore {
    /// Load from `path` with the startup fallback rules (missing or invalid →
    /// defaults). `path: None` (no `APPDATA`) also means defaults.
    pub fn load(path: Option<PathBuf>) -> Self {
        let config = match &path {
            None => {
                log::warn!("APPDATA not set; using default config");
                Config::default()
            }
            Some(p) => match read(p) {
                Ok(Some(config)) => {
                    log::info!("loaded config from {}", p.display());
                    config
                }
                Ok(None) => {
                    log::info!("no config at {}; using defaults", p.display());
                    Config::default()
                }
                Err(e) => {
                    log::error!("config at {} is broken; using defaults: {e:#}", p.display());
                    Config::default()
                }
            },
        };
        log::info!(
            "config: wallpaper color {}, image {:?}",
            config.wallpaper.color,
            config.wallpaper.image
        );
        ConfigStore {
            path,
            current: RwLock::new(config),
        }
    }

    /// Load from [`default_path`].
    pub fn load_default() -> Self {
        Self::load(default_path())
    }

    /// A snapshot of the current config (it is small; a clone keeps the lock
    /// scope out of callers' hands).
    pub fn get(&self) -> Config {
        self.current.read().expect("config lock").clone()
    }

    /// Re-read the file (the `ReloadConfig` path). Missing file → defaults
    /// (the file's absence is what it now says); broken file → keep what we
    /// have. Returns the config now in effect.
    pub fn reload(&self) -> Config {
        let Some(path) = &self.path else {
            log::warn!("reload requested but APPDATA is not set; keeping current config");
            return self.get();
        };
        let new = match read(path) {
            Ok(Some(config)) => {
                log::info!("reloaded config from {}", path.display());
                config
            }
            Ok(None) => {
                log::info!(
                    "config at {} removed; reverting to defaults",
                    path.display()
                );
                Config::default()
            }
            Err(e) => {
                log::error!(
                    "reload failed; keeping current config: {e:#} ({})",
                    path.display()
                );
                return self.get();
            }
        };
        log::info!(
            "config now: wallpaper color {}, image {:?}",
            new.wallpaper.color,
            new.wallpaper.image
        );
        *self.current.write().expect("config lock") = new.clone();
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_missing_fields_take_defaults() {
        for text in ["", "[wallpaper]", "[wallpaper]\ncolor = \"#010203\""] {
            let config: Config = toml::from_str(text).unwrap();
            assert_eq!(config.wallpaper.image, None, "{text:?}");
        }
        assert_eq!(toml::from_str::<Config>("").unwrap(), Config::default());
    }

    #[test]
    fn autostart_defaults_mirror_windows() {
        let autostart = toml::from_str::<Config>("").unwrap().autostart;
        assert!(autostart.enabled);
        assert!(autostart.allows("hkcu-run:Anything"));
    }

    #[test]
    fn autostart_disabled_list_is_case_insensitive() {
        let config: Config =
            toml::from_str("[autostart]\ndisabled = [\"HKCU-Run:OneDrive\"]\n").unwrap();
        assert!(!config.autostart.allows("hkcu-run:onedrive"));
        assert!(config.autostart.allows("hkcu-run:other"));
    }

    #[test]
    fn autostart_master_switch_blocks_everything() {
        let config: Config = toml::from_str("[autostart]\nenabled = false\n").unwrap();
        assert!(!config.autostart.allows("hkcu-run:anything"));
    }

    #[test]
    fn full_config_parses() {
        let config: Config =
            toml::from_str("[wallpaper]\ncolor = \"#A1b2C3\"\nimage = 'C:\\Users\\me\\bg.jpg'\n")
                .unwrap();
        assert_eq!(
            config.wallpaper.color,
            Color {
                r: 0xa1,
                g: 0xb2,
                b: 0xc3
            }
        );
        assert_eq!(
            config.wallpaper.image.as_deref(),
            Some(Path::new(r"C:\Users\me\bg.jpg"))
        );
    }

    #[test]
    fn taskbar_defaults_are_sane() {
        let taskbar = toml::from_str::<Config>("").unwrap().taskbar;
        assert!(taskbar.enabled);
        assert!(taskbar.height > 0);
        assert_eq!(taskbar, Taskbar::default());
    }

    #[test]
    fn taskbar_section_parses() {
        let config: Config = toml::from_str(
            "[taskbar]\nenabled = false\nheight = 56\ncolor = \"#334455\"\n\
             alpha = 128\ncorner_radius = 0\nmargin = 0\n",
        )
        .unwrap();
        assert!(!config.taskbar.enabled);
        assert_eq!(config.taskbar.height, 56);
        assert_eq!(config.taskbar.color.to_string(), "#334455");
        assert_eq!(config.taskbar.alpha, 128);
        assert_eq!(config.taskbar.corner_radius, 0);
        assert_eq!(config.taskbar.margin, 0);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config: Config =
            toml::from_str("from_the_future = 1\n[wallpaper]\nshine = true\n").unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn bad_colors_are_parse_errors() {
        for bad in ["", "#fff", "#12345", "#1234567", "1a1a2e", "#gggggg"] {
            assert!(bad.parse::<Color>().is_err(), "{bad:?}");
            assert!(
                toml::from_str::<Config>(&format!("[wallpaper]\ncolor = {bad:?}")).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn color_round_trips_through_display() {
        let color: Color = "#1A2b3C".parse().unwrap();
        assert_eq!(color.to_string(), "#1a2b3c");
        assert_eq!(color.to_string().parse::<Color>().unwrap(), color);
    }

    /// A scratch config path unique to this test.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wr-config-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn store_defaults_when_file_is_missing_or_path_unknown() {
        assert_eq!(
            ConfigStore::load(Some(scratch("missing"))).get(),
            Config::default()
        );
        assert_eq!(ConfigStore::load(None).get(), Config::default());
    }

    #[test]
    fn store_defaults_on_broken_file_at_startup() {
        let path = scratch("broken-start");
        std::fs::write(&path, "wallpaper = 3").unwrap();
        assert_eq!(ConfigStore::load(Some(path)).get(), Config::default());
    }

    #[test]
    fn reload_swaps_good_keeps_current_on_broken_defaults_on_missing() {
        let path = scratch("reload");
        let store = ConfigStore::load(Some(path.clone()));

        std::fs::write(&path, "[wallpaper]\ncolor = \"#010203\"").unwrap();
        let loaded = store.reload();
        assert_eq!(loaded.wallpaper.color.to_string(), "#010203");
        assert_eq!(store.get(), loaded);

        // Broken file: the good config stays in effect.
        std::fs::write(&path, "[wallpaper]\ncolor = \"oops").unwrap();
        assert_eq!(store.reload(), loaded);
        assert_eq!(store.get(), loaded);

        // Removed file: back to defaults.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(store.reload(), Config::default());
    }

    #[test]
    fn default_path_is_under_appdata() {
        // The only test that touches APPDATA, so no env races with the others.
        std::env::set_var("APPDATA", r"C:\Users\me\AppData\Roaming");
        // Built with `join` so the test also passes on the Linux dev host,
        // where the separator differs.
        assert_eq!(
            default_path(),
            Some(
                PathBuf::from(r"C:\Users\me\AppData\Roaming")
                    .join("WinRestyle")
                    .join("config.toml")
            )
        );
    }
}
