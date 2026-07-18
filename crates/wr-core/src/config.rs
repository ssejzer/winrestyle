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
