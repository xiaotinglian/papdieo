mod cli;
mod config;
mod picker;
mod wallpaper;

use anyhow::{anyhow, Result};
use clap::Parser;
use std::{
    fs::OpenOptions,
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::Duration,
};

use cli::{Command, PapdieoArgs};
use config::FitMode;

fn main() -> Result<()> {
    let args = PapdieoArgs::parse();
    run(args)
}

fn run(args: PapdieoArgs) -> Result<()> {
    let config = config::Config::load_or_default(args.config.as_deref())?;
    let default_fps = config.video_fps.unwrap_or(60);
    let default_fit = config.fit_mode.unwrap_or(FitMode::Cover);
    let default_interval = config.rotation_seconds.unwrap_or(300);

    match args.command {
        Command::Set {
            path,
            monitor,
            fps,
            fit,
            detach,
        } => run_renderer(
            path,
            monitor.or_else(|| config.monitor.clone()),
            fps.unwrap_or(default_fps),
            fit.unwrap_or(default_fit),
            detach,
        ),
        Command::Random {
            dir,
            monitor,
            fps,
            fit,
            detach,
        } => {
            let media_dir = dir.unwrap_or_else(|| config.wallpaper_dir.clone());
            let image = picker::pick_random_wallpaper(&media_dir)?;
            run_renderer(
                image,
                monitor.or_else(|| config.monitor.clone()),
                fps.unwrap_or(default_fps),
                fit.unwrap_or(default_fit),
                detach,
            )
        }
        Command::Next {
            dir,
            monitor,
            fps,
            fit,
            detach,
        } => {
            let media_dir = dir.unwrap_or_else(|| config.wallpaper_dir.clone());
            let image = picker::pick_next_wallpaper(&media_dir)?;
            run_renderer(
                image,
                monitor.or_else(|| config.monitor.clone()),
                fps.unwrap_or(default_fps),
                fit.unwrap_or(default_fit),
                detach,
            )
        }
        Command::Rotate {
            dir,
            monitor,
            interval,
            fps,
            fit,
        } => run_rotate_loop(
            dir.unwrap_or_else(|| config.wallpaper_dir.clone()),
            monitor.or_else(|| config.monitor.clone()),
            interval.unwrap_or(default_interval),
            fps.unwrap_or(default_fps),
            fit.unwrap_or(default_fit),
        ),
        Command::List => {
            let images = picker::list_wallpapers(&config.wallpaper_dir)?;
            for img in images {
                println!("{}", img.display());
            }
            Ok(())
        }
        Command::__RunInternal {
            path,
            monitor,
            fps,
            fit,
        } => wallpaper::run_wallpaper(
            path,
            monitor.as_deref(),
            fps.unwrap_or(default_fps),
            fit.unwrap_or(default_fit),
        ),
    }
}

fn run_renderer(
    path: std::path::PathBuf,
    monitor: Option<String>,
    fps: u32,
    fit: FitMode,
    detach: bool,
) -> Result<()> {
    if !detach {
        return wallpaper::run_wallpaper(path, monitor.as_deref(), fps, fit);
    }

    let exe = std::env::current_exe()?;
    let log_path = "/tmp/papdieo.log";
    let log_out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(log_path)?;
    let log_err = log_out.try_clone()?;

    let mut child = ProcessCommand::new(exe)
        .arg("run-internal")
        .arg(&path)
        .args(monitor.as_ref().map(|m| vec!["--monitor", m]).unwrap_or_default())
        .arg("--fps")
        .arg(fps.to_string())
        .arg("--fit")
        .arg(match fit {
            FitMode::Stretch => "stretch",
            FitMode::Fill => "fill",
            FitMode::Cover => "cover",
            FitMode::Fit => "fit",
            FitMode::Contain => "contain",
        })
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .spawn()?;

    thread::sleep(Duration::from_millis(4000));
    if let Some(status) = child.try_wait()? {
        return Err(anyhow!(
            "wallpaper renderer exited early (status: {}), see {}",
            status,
            log_path
        ));
    }

    println!(
        "Started wallpaper renderer in background (pid: {}, log: {})",
        child.id(),
        log_path
    );
    Ok(())
}

fn run_rotate_loop(
    media_dir: std::path::PathBuf,
    monitor: Option<String>,
    interval_seconds: u64,
    fps: u32,
    fit: FitMode,
) -> Result<()> {
    let interval = std::time::Duration::from_secs(interval_seconds.max(1));

    loop {
        let _ = ProcessCommand::new("pkill")
            .arg("-f")
            .arg("papdieo run-internal")
            .status();

        let media = picker::pick_random_wallpaper(&media_dir)?;
        let exe = std::env::current_exe()?;

        ProcessCommand::new(&exe)
            .arg("run-internal")
            .arg(&media)
            .args(monitor.as_ref().map(|m| vec!["--monitor", m]).unwrap_or_default())
            .arg("--fps")
            .arg(fps.to_string())
            .arg("--fit")
            .arg(match fit {
                FitMode::Stretch => "stretch",
                FitMode::Fill => "fill",
                FitMode::Cover => "cover",
                FitMode::Fit => "fit",
                FitMode::Contain => "contain",
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        thread::sleep(interval);
    }
}
