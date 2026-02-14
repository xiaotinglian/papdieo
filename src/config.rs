use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Deserialize;
use std::{
    collections::HashMap,
    env,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_wallpaper_dir")]
    pub wallpaper_dir: PathBuf,
    pub monitor_wallpaper_dirs: Option<HashMap<String, PathBuf>>,
    pub monitor: Option<String>,
    pub monitors: Option<Vec<String>>,
    pub video_fps: Option<u32>,
    pub rotation_seconds: Option<u64>,
    pub daemon_interval_seconds: Option<u64>,
    pub fit_mode: Option<FitMode>,
}

#[derive(Debug, Clone, Copy, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    Stretch,
    Fill,
    Cover,
    Fit,
    Contain,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wallpaper_dir: default_wallpaper_dir(),
            monitor_wallpaper_dirs: None,
            monitor: None,
            monitors: None,
            video_fps: Some(60),
            rotation_seconds: Some(300),
            daemon_interval_seconds: Some(300),
            fit_mode: Some(FitMode::Cover),
        }
    }
}

fn default_wallpaper_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Pictures").join("Wallpapers")
}

impl Config {
    pub fn load_or_default(config_path: Option<&Path>) -> Result<Self> {
        let path = if let Some(path) = config_path {
            path.to_path_buf()
        } else if let Some(default_path) = default_config_path() {
            if default_path.exists() {
                default_path
            } else {
                return Ok(Self::default());
            }
        } else {
            return Ok(Self::default());
        };

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;

        toml::from_str(&content)
            .with_context(|| format!("failed to parse TOML config: {}", path.display()))
    }
}

fn default_config_path() -> Option<PathBuf> {
    let base = env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;

    Some(base.join("papdieo").join("config.toml"))
}
