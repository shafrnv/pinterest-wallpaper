use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedSource {
    Homefeed,
    AllBoards,
    Board { id: String },
    /// Local image directory for offline / UI testing
    Local { path: PathBuf },
}

impl Default for FeedSource {
    fn default() -> Self {
        Self::Homefeed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthConfig {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_redirect")]
    pub redirect_uri: String,
}

fn default_redirect() -> String {
    "http://127.0.0.1:8765/callback".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub feed: FeedSource,
    #[serde(default = "default_columns")]
    pub columns: u32,
    #[serde(default = "default_gap")]
    pub gap: i32,
    #[serde(default = "default_bg")]
    pub background: String,
    /// Layer: "background" (true wallpaper) or "bottom" (easier input)
    #[serde(default = "default_layer")]
    pub layer: String,
    /// Positions of monitors on one scrolling canvas.
    /// Empty means "use the compositor layout".
    #[serde(default)]
    pub monitors: Vec<MonitorPlacement>,
    #[serde(default)]
    pub keys: KeysConfig,
    pub oauth: Option<OauthConfig>,
}

/// Wheel zoom and keyboard panning. Movement keys apply only while the cursor
/// is on the wallpaper, not over a window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeysConfig {
    /// Modifier held with the wheel. `ctrl`, `alt`, `super`, `shift`, or `ctrl+shift`.
    #[serde(default = "default_zoom_mod")]
    pub zoom: String,
    /// How much one wheel notch multiplies the scale. 1.1 is ten percent.
    #[serde(default = "default_zoom_step")]
    pub zoom_step: f64,
    /// Screen pixels per second while a move key is held.
    #[serde(default = "default_move_speed")]
    pub move_speed: f64,
    #[serde(default = "default_left", deserialize_with = "one_or_many")]
    pub left: Vec<String>,
    #[serde(default = "default_right", deserialize_with = "one_or_many")]
    pub right: Vec<String>,
    /// Up the feed.
    #[serde(default = "default_up", deserialize_with = "one_or_many")]
    pub up: Vec<String>,
    #[serde(default = "default_down", deserialize_with = "one_or_many")]
    pub down: Vec<String>,
}

fn default_zoom_mod() -> String {
    "ctrl".into()
}

fn one_or_many<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(value) => vec![value],
        OneOrMany::Many(values) => values,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomMod {
    Ctrl,
    Alt,
    Shift,
    Super,
}

impl KeysConfig {
    pub fn zoom_mods(&self) -> Vec<ZoomMod> {
        let mods: Vec<ZoomMod> = self
            .zoom
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter_map(|part| match ZoomMod::parse(part) {
                Some(modifier) => Some(modifier),
                None => {
                    tracing::warn!("неизвестный модификатор зума: {part}");
                    None
                }
            })
            .collect();
        if mods.is_empty() {
            vec![ZoomMod::Ctrl]
        } else {
            mods
        }
    }

    pub fn zoom_step(&self) -> f64 {
        if self.zoom_step.is_finite() && self.zoom_step > 1.0 {
            self.zoom_step.min(3.0)
        } else {
            default_zoom_step()
        }
    }

    pub fn move_speed(&self) -> f64 {
        if self.move_speed.is_finite() && self.move_speed > 0.0 {
            self.move_speed.min(6000.0)
        } else {
            default_move_speed()
        }
    }
}

impl ZoomMod {
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(Self::Ctrl),
            "alt" => Some(Self::Alt),
            "shift" => Some(Self::Shift),
            "super" | "meta" | "win" => Some(Self::Super),
            _ => None,
        }
    }
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            zoom: default_zoom_mod(),
            zoom_step: default_zoom_step(),
            move_speed: default_move_speed(),
            left: default_left(),
            right: default_right(),
            up: default_up(),
            down: default_down(),
        }
    }
}

fn default_zoom_step() -> f64 {
    1.1
}

fn default_move_speed() -> f64 {
    800.0
}

fn default_left() -> Vec<String> {
    vec!["Left".into(), "a".into()]
}

fn default_right() -> Vec<String> {
    vec!["Right".into(), "d".into()]
}

fn default_up() -> Vec<String> {
    vec!["Up".into(), "w".into()]
}

fn default_down() -> Vec<String> {
    vec!["Down".into(), "s".into()]
}

/// XKB keycode (evdev code + 8), which is what Hyprland's Lua key event reports.
pub fn xkb_keycode(name: &str) -> Option<i32> {
    let key = name.trim().to_ascii_lowercase().replace('-', "_");
    let evdev = match key.as_str() {
        "left" | "arrow_left" => 105,
        "right" | "arrow_right" => 106,
        "up" | "arrow_up" => 103,
        "down" | "arrow_down" => 108,
        "minus" | "kp_minus" => 12,
        "equal" | "plus" | "kp_plus" => 13,
        "a" => 30,
        "b" => 48,
        "c" => 46,
        "d" => 32,
        "e" => 18,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "i" => 23,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "m" => 50,
        "n" => 49,
        "o" => 24,
        "p" => 25,
        "q" => 16,
        "r" => 19,
        "s" => 31,
        "t" => 20,
        "u" => 22,
        "v" => 47,
        "w" => 17,
        "x" => 45,
        "y" => 21,
        "z" => 44,
        "space" => 57,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        _ => return None,
    };
    Some(evdev + 8)
}

pub fn xkb_keycodes(names: &[String]) -> Vec<i32> {
    let mut codes = Vec::new();
    for name in names {
        match xkb_keycode(name) {
            Some(code) if !codes.contains(&code) => codes.push(code),
            Some(_) => {}
            None => tracing::warn!("неизвестная клавиша в конфиге: {name}"),
        }
    }
    codes
}

fn default_columns() -> u32 {
    5
}

fn default_gap() -> i32 {
    8
}

fn default_bg() -> String {
    "#111111".into()
}

fn default_layer() -> String {
    "background".into()
}

/// One cell of the shared wallpaper canvas.
/// `x` and `y` are pixels from the top-left of that canvas.
/// Width and height come from the real monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorPlacement {
    /// Connector name, as in `hyprctl monitors` (`HDMI-A-1`, `DP-1`, …).
    pub name: String,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            feed: FeedSource::Homefeed,
            columns: default_columns(),
            gap: default_gap(),
            background: default_bg(),
            layer: default_layer(),
            monitors: Vec::new(),
            keys: KeysConfig::default(),
            oauth: None,
        }
    }
}

impl Config {
    pub fn config_dir() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .context("no config dir")?
            .join("pinterest-wallpaper");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn cache_dir() -> Result<PathBuf> {
        let dir = dirs::cache_dir()
            .context("no cache dir")?
            .join("pinterest-wallpaper");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            let cfg = Self::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).context("parse config.toml")?;
        Ok(cfg)
    }

    #[cfg(test)]
    fn parse(text: &str) -> Self {
        toml::from_str(text).expect("config")
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let text = toml::to_string_pretty(self)?;
        fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_keys_keep_wasd_and_arrows() {
        let cfg = Config::parse(
            r#"
            [keys]
            zoom = "super"
            zoom_step = 1.25
            "#,
        );
        assert_eq!(cfg.keys.zoom, "super");
        assert!((cfg.keys.zoom_step() - 1.25).abs() < 1e-9);
        assert_eq!(cfg.keys.left, vec!["Left".to_string(), "a".to_string()]);
        assert_eq!(cfg.keys.up, vec!["Up".to_string(), "w".to_string()]);
        assert_eq!(cfg.keys.zoom_mods(), vec![ZoomMod::Super]);
        assert_eq!(xkb_keycode("a"), Some(38));
        assert_eq!(xkb_keycode("Left"), Some(113));
        assert_eq!(xkb_keycode("w"), Some(25));
    }
}
