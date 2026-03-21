use clap::{Parser, Subcommand};
use crate::config::FitMode;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "papdieo", version, about = "A Hyprland-compatible wallpaper CLI")]
pub struct PapdieoArgs {
    #[arg(short, long, help = "Path to config TOML file")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "Start wallpaper daemon service")]
    Daemon {
        #[arg(long, help = "Run daemon in foreground (no detach)")]
        foreground: bool,
    },

    #[command(about = "Restart wallpaper daemon service")]
    Restart,

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
        #[arg(required_unless_present = "assignments")]
        path: Option<PathBuf>,
        #[arg(long)]
        assignments: Option<String>,
        #[arg(long)]
        monitor: Option<String>,
        #[arg(long)]
        fps: Option<u32>,
        #[arg(long, value_enum)]
        fit: Option<FitMode>,
    },

    #[command(hide = true)]
    __DaemonInternal,
}

#[cfg(test)]
mod tests {
    use super::{Command, PapdieoArgs};
    use clap::Parser;

    #[test]
    fn run_internal_accepts_assignments_without_path() {
        let args = PapdieoArgs::try_parse_from([
            "papdieo",
            "run-internal",
            "--assignments",
            "[{\"monitor\":\"DP-1\",\"path\":\"/tmp/a.png\",\"fit\":\"cover\"}]",
            "--fps",
            "60",
        ])
        .expect("run-internal with assignments should parse");

        match args.command {
            Some(Command::__RunInternal {
                path,
                assignments,
                fps,
                ..
            }) => {
                assert!(path.is_none());
                assert!(assignments.is_some());
                assert_eq!(fps, Some(60));
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn run_internal_accepts_legacy_path_mode() {
        let args = PapdieoArgs::try_parse_from([
            "papdieo",
            "run-internal",
            "/tmp/a.png",
            "--monitor",
            "DP-1",
            "--fps",
            "30",
        ])
        .expect("run-internal with path should parse");

        match args.command {
            Some(Command::__RunInternal {
                path,
                assignments,
                monitor,
                fps,
                ..
            }) => {
                assert_eq!(path.as_deref().and_then(|p| p.to_str()), Some("/tmp/a.png"));
                assert!(assignments.is_none());
                assert_eq!(monitor.as_deref(), Some("DP-1"));
                assert_eq!(fps, Some(30));
            }
            _ => panic!("unexpected command variant"),
        }
    }
}
