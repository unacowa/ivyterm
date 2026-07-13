use std::{
    fs,
    path::{Path, PathBuf},
};

use gtk4::{gdk::RGBA, pango::FontDescription};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
pub use terminal::{ColorScheme, TerminalConfig};
use tmux::TmuxConfig;

use crate::keyboard::Keybindings;

mod terminal;
mod tmux;

pub const INITIAL_WIDTH: i32 = 802;
pub const INITIAL_HEIGHT: i32 = 648;
pub const APPLICATION_TITLE: &str = "ivyTerm";
pub const SPLIT_HANDLE_WIDTH: i32 = 10;
pub const SPLIT_VISUAL_WIDTH: i32 = 2;

#[derive(Deserialize, Serialize, Clone)]
pub struct GlobalConfig {
    #[serde(default, skip)]
    path: Option<PathBuf>,
    #[serde(default)]
    pub terminal: TerminalConfig,
    #[serde(default)]
    pub tmux: TmuxConfig,
    #[serde(default)]
    pub keybindings: Keybindings,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        // Load user config; any failure falls back to the built-in
        // defaults instead of aborting the application at startup
        let Some(home_dir) = dirs::home_dir() else {
            eprintln!("Cannot determine home directory, configuration will not be persisted");
            return Self::builtin();
        };

        let parent_dir = home_dir.join(".config").join("ivyterm");
        let config_path = parent_dir.join("config.toml");

        if config_path.exists() {
            match Self::load_from_file(&config_path) {
                Ok(mut config) => {
                    // Store the path in config, so we don't have to determine it every time
                    config.path = Some(config_path);
                    // Write parsed config back to the same path
                    config.write_config_to_file();
                    config
                }
                Err(err) => {
                    eprintln!("Error loading {}: {}", config_path.display(), err);
                    eprintln!("Using default configuration (the file is left untouched)");
                    let mut config = Self::builtin();
                    // Keep the path so explicit changes made in the settings
                    // window still persist, but do not overwrite the broken
                    // file just because the application started
                    config.path = Some(config_path);
                    config
                }
            }
        } else {
            let mut config = Self::builtin();
            // We know we will be writing config back to file, ensure the parent directory exists
            match fs::create_dir_all(&parent_dir) {
                Ok(()) => {
                    config.path = Some(config_path);
                    config.write_config_to_file();
                }
                Err(err) => {
                    // e.g. ~/.config/ivyterm exists as a regular file
                    eprintln!(
                        "Cannot create config directory {}: {}",
                        parent_dir.display(),
                        err
                    );
                }
            }
            config
        }
    }
}

impl GlobalConfig {
    /// The built-in default configuration (as if config.toml were empty).
    /// Guarded by a unit test, hence the expect cannot trigger in practice
    fn builtin() -> Self {
        toml::from_str("").expect("the built-in default config must be valid")
    }

    /// Reads and parses a config.toml. All errors are reported as strings,
    /// so callers can log them and fall back to defaults
    fn load_from_file(path: &Path) -> Result<GlobalConfig, String> {
        let content = fs::read_to_string(path).map_err(|err| err.to_string())?;
        toml::from_str(&content).map_err(|err| err.to_string())
    }

    pub fn write_config_to_file(&self) {
        // Filesystem is always done async
        if let Some(path) = &self.path {
            let path = path.clone();
            let toml = match toml::to_string(self) {
                Ok(toml) => toml,
                Err(err) => {
                    eprintln!("Cannot serialize configuration: {}", err);
                    return;
                }
            };

            glib::spawn_future_local(async move {
                if let Err(err) = fs::write(&path, toml.as_bytes()) {
                    eprintln!("Unable to write config file {}: {}", path.display(), err);
                }
            });
        }
    }
}

#[derive(Clone)]
pub struct IvyColor(RGBA);

impl<'de> Deserialize<'de> for IvyColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hex = String::deserialize(deserializer)?;
        match RGBA::parse(&hex) {
            Ok(rgba) => Ok(IvyColor(rgba)),
            Err(err) => Err(serde::de::Error::custom(format!(
                "invalid color {:?}: {}",
                hex, err
            ))),
        }
    }
}

impl Serialize for IvyColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let string = self.to_hex();
        serializer.serialize_str(&string)
    }
}

impl IvyColor {
    pub fn to_hex(&self) -> String {
        let rgba = self.0;
        let red = (rgba.red() * 255.).round() as i32;
        let green = (rgba.green() * 255.).round() as i32;
        let blue = (rgba.blue() * 255.).round() as i32;
        format!("#{:02X}{:02X}{:02X}", red, green, blue)
    }
}

impl From<RGBA> for IvyColor {
    fn from(value: RGBA) -> Self {
        Self(value)
    }
}

impl Into<RGBA> for IvyColor {
    fn into(self) -> RGBA {
        self.0
    }
}

impl AsRef<RGBA> for IvyColor {
    fn as_ref(&self) -> &RGBA {
        &self.0
    }
}

#[derive(Clone, Default)]
pub struct IvyFont(FontDescription);

impl IvyFont {
    pub fn new(font: &str) -> Self {
        let font = FontDescription::from_string(font);
        Self(font)
    }
}

impl<'de> Deserialize<'de> for IvyFont {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let font_description = String::deserialize(deserializer)?;
        let font_description = FontDescription::from_string(&font_description);
        Ok(IvyFont(font_description))
    }
}

impl Serialize for IvyFont {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let string = self.0.to_str();
        serializer.serialize_str(&string)
    }
}

impl From<FontDescription> for IvyFont {
    fn from(value: FontDescription) -> Self {
        Self(value)
    }
}

impl Into<FontDescription> for IvyFont {
    fn into(self) -> FontDescription {
        self.0
    }
}

impl AsRef<FontDescription> for IvyFont {
    fn as_ref(&self) -> &FontDescription {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Unique temp file path per test (no extra dev-dependencies)
    fn temp_config_path() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ivyterm-config-test-{}-{}.toml",
            std::process::id(),
            unique
        ))
    }

    /// Guards the expect() inside GlobalConfig::builtin()
    #[test]
    fn builtin_config_is_valid() {
        let config = GlobalConfig::builtin();
        assert_eq!(config.terminal.scrollback_lines, 500);
        assert!(config.path.is_none());
    }

    /// Issue #6: malformed TOML used to abort the app via unwrap()
    #[test]
    fn malformed_toml_is_an_error_not_a_panic() {
        assert!(toml::from_str::<GlobalConfig>("this is { not toml").is_err());
    }

    /// Issue #6: a wrong value type used to abort the app via unwrap()
    #[test]
    fn wrong_value_type_is_an_error_not_a_panic() {
        let result = toml::from_str::<GlobalConfig>("[terminal]\nscrollback_lines = \"abc\"");
        assert!(result.is_err());
    }

    /// Issue #6: IvyColor::deserialize used to call panic!() directly on
    /// any color RGBA cannot parse
    #[test]
    fn invalid_color_is_an_error_not_a_panic() {
        let result = toml::from_str::<GlobalConfig>("[terminal]\nforeground = \"notacolor\"");
        let err = result.err().expect("invalid color must be a serde error");
        assert!(
            err.to_string().contains("invalid color"),
            "the error should name the problem, got: {}",
            err
        );
    }

    /// A partial config keeps defaults for everything not mentioned
    #[test]
    fn partial_config_fills_in_defaults() {
        let config: GlobalConfig =
            toml::from_str("[terminal]\nscrollback_lines = 9000").unwrap();
        assert_eq!(config.terminal.scrollback_lines, 9000);
        assert_eq!(config.terminal.foreground.to_hex(), "#FFFFFF");
    }

    /// What write_config_to_file persists must load back unchanged
    #[test]
    fn serialized_config_roundtrips() {
        let mut config = GlobalConfig::builtin();
        config.terminal.scrollback_lines = 1234;

        let toml = toml::to_string(&config).expect("config must serialize");
        let reloaded: GlobalConfig = toml::from_str(&toml).expect("serialized config must parse");
        assert_eq!(reloaded.terminal.scrollback_lines, 1234);
        assert_eq!(
            reloaded.terminal.background.to_hex(),
            config.terminal.background.to_hex()
        );
    }

    #[test]
    fn load_from_file_reads_valid_config() {
        let path = temp_config_path();
        fs::write(&path, "[terminal]\nscrollback_lines = 777\n").unwrap();

        let config = GlobalConfig::load_from_file(&path).expect("valid file must load");
        assert_eq!(config.terminal.scrollback_lines, 777);

        let _ = fs::remove_file(&path);
    }

    /// Issue #6: an unreadable or missing file must be a reported error,
    /// not an unwrap panic
    #[test]
    fn load_from_file_reports_missing_file() {
        let path = temp_config_path();
        assert!(GlobalConfig::load_from_file(&path).is_err());
    }

    #[test]
    fn load_from_file_reports_broken_file() {
        let path = temp_config_path();
        fs::write(&path, "[terminal\nbroken").unwrap();

        assert!(GlobalConfig::load_from_file(&path).is_err());

        let _ = fs::remove_file(&path);
    }

    /// Non-UTF-8 content used to abort in fs::read_to_string().unwrap()
    #[test]
    fn load_from_file_reports_non_utf8_file() {
        let path = temp_config_path();
        fs::write(&path, [0xff, 0xfe, 0x00, 0x01]).unwrap();

        assert!(GlobalConfig::load_from_file(&path).is_err());

        let _ = fs::remove_file(&path);
    }
}
