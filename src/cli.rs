use clap::{Parser, Subcommand};
use crate::config::FitMode;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "papdieo", version, about = "A Hyprland-compatible wallpaper CLI")]
pub struct PapdieoArgs {
    #[arg(short, long, help = "Path to config TOML file")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "Set a specific wallpaper")]
    Set {
        path: PathBuf,
        #[arg(long, help = "Target monitor name (example: DP-4)")]
        monitor: Option<String>,
        #[arg(long, help = "Video FPS target (default: 60)")]
        fps: Option<u32>,
        #[arg(long, value_enum, help = "Render mode: stretch|fill|cover|fit|contain")]
        fit: Option<FitMode>,
        #[arg(long, help = "Run wallpaper renderer in background")]
        detach: bool,
    },

    #[command(about = "Pick a random wallpaper from configured directory")]
    Random {
        #[arg(long, help = "Media directory override")]
        dir: Option<PathBuf>,
        #[arg(long, help = "Target monitor name (example: DP-4)")]
        monitor: Option<String>,
        #[arg(long, help = "Video FPS target (default: 60)")]
        fps: Option<u32>,
        #[arg(long, value_enum, help = "Render mode: stretch|fill|cover|fit|contain")]
        fit: Option<FitMode>,
        #[arg(long, help = "Run wallpaper renderer in background")]
        detach: bool,
    },

    #[command(about = "Pick next wallpaper in sorted order")]
    Next {
        #[arg(long, help = "Media directory override")]
        dir: Option<PathBuf>,
        #[arg(long, help = "Target monitor name (example: DP-4)")]
        monitor: Option<String>,
        #[arg(long, help = "Video FPS target (default: 60)")]
        fps: Option<u32>,
        #[arg(long, value_enum, help = "Render mode: stretch|fill|cover|fit|contain")]
        fit: Option<FitMode>,
        #[arg(long, help = "Run wallpaper renderer in background")]
        detach: bool,
    },

    #[command(about = "Continuously rotate random wallpapers/videos from a folder")]
    Rotate {
        #[arg(long, help = "Media directory override")]
        dir: Option<PathBuf>,
        #[arg(long, help = "Target monitor name (example: DP-4)")]
        monitor: Option<String>,
        #[arg(long, help = "Rotation interval in seconds")]
        interval: Option<u64>,
        #[arg(long, help = "Video FPS target (default: 60)")]
        fps: Option<u32>,
        #[arg(long, value_enum, help = "Render mode: stretch|fill|cover|fit|contain")]
        fit: Option<FitMode>,
    },

    #[command(about = "List discovered wallpapers")]
    List,

    #[command(hide = true)]
    __RunInternal {
        path: PathBuf,
        #[arg(long)]
        monitor: Option<String>,
        #[arg(long)]
        fps: Option<u32>,
        #[arg(long, value_enum)]
        fit: Option<FitMode>,
    },
}
