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
    pub oauth: Option<OauthConfig>,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            feed: FeedSource::Homefeed,
            columns: default_columns(),
            gap: default_gap(),
            background: default_bg(),
            layer: default_layer(),
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

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let text = toml::to_string_pretty(self)?;
        fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}
