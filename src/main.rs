mod cli;
mod config;
mod picker;
mod wallpaper;

use anyhow::{anyhow, Result};
use clap::Parser;
use fs2::FileExt;
use std::{
    collections::HashMap,
    env,
    fs::File,
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, SystemTime},
};

use cli::{Command, PapdieoArgs};
use config::FitMode;

const DAEMON_PID_PATH: &str = "/tmp/papdieo-daemon.pid";
const DAEMON_LOG_PATH: &str = "/tmp/papdieo-daemon.log";
const DAEMON_LOCK_PATH: &str = "/tmp/papdieo-daemon.lock";

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
        None => start_daemon_service(args.config.as_deref()),
        Some(Command::Daemon { foreground }) => {
            if foreground {
                run_daemon_loop(args.config.as_deref())
            } else {
                start_daemon_service(args.config.as_deref())
            }
        }
        Some(Command::Set {
            path,
            monitor,
            fps,
            fit,
            detach,
        }) => run_renderer(
            path,
            monitor.or_else(|| config.monitor.clone()),
            fps.unwrap_or(default_fps),
            fit.unwrap_or(default_fit),
            detach,
        ),
        Some(Command::Random {
            dir,
            monitor,
            fps,
            fit,
            detach,
        }) => {
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
        Some(Command::Next {
            dir,
            monitor,
            fps,
            fit,
            detach,
        }) => {
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
        Some(Command::Rotate {
            dir,
            monitor,
            interval,
            fps,
            fit,
        }) => run_rotate_loop(
            dir.unwrap_or_else(|| config.wallpaper_dir.clone()),
            monitor.or_else(|| config.monitor.clone()),
            interval.unwrap_or(default_interval),
            fps.unwrap_or(default_fps),
            fit.unwrap_or(default_fit),
        ),
        Some(Command::List) => {
            let images = picker::list_wallpapers(&config.wallpaper_dir)?;
            for img in images {
                println!("{}", img.display());
            }
            Ok(())
        }
        Some(Command::__RunInternal {
            path,
            monitor,
            fps,
            fit,
        }) => wallpaper::run_wallpaper(
            path,
            monitor.as_deref(),
            fps.unwrap_or(default_fps),
            fit.unwrap_or(default_fit),
        ),
        Some(Command::__DaemonInternal) => run_daemon_loop(args.config.as_deref()),
    }
}

fn start_daemon_service(config_path: Option<&Path>) -> Result<()> {
    let pid_path = Path::new(DAEMON_PID_PATH);
    if daemon_is_running(pid_path) {
        println!("papdieo daemon already running");
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let log_path = DAEMON_LOG_PATH;
    let log_out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let log_err = log_out.try_clone()?;

    let mut command = ProcessCommand::new(exe);
    if let Some(path) = config_path {
        command.arg("--config").arg(path);
    }

    let mut child = command
        .arg("daemon-internal")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .spawn()?;

    thread::sleep(Duration::from_millis(350));
    if let Some(status) = child.try_wait()? {
        return Err(anyhow!(
            "failed to start daemon (status: {}), see {}",
            status,
            DAEMON_LOG_PATH
        ));
    }

    std::fs::write(pid_path, child.id().to_string())?;
    println!(
        "Started papdieo daemon (pid: {}, log: {})",
        child.id(),
        log_path
    );
    Ok(())
}

fn daemon_is_running(pid_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(pid_path) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<u32>() else {
        return false;
    };

    PathBuf::from(format!("/proc/{pid}")).exists()
}

fn run_daemon_loop(config_path: Option<&Path>) -> Result<()> {
    let _daemon_lock = acquire_daemon_lock()?;

    let mut monitor_children: HashMap<String, Child> = HashMap::new();
    let watched_config_path = resolve_config_watch_path(config_path);
    let mut observed_config_mtime = watched_config_path
        .as_ref()
        .and_then(|path| config_file_modified_time(path));

    loop {
        let cfg = config::Config::load_or_default(config_path)?;
        let fps = cfg.video_fps.unwrap_or(60);
        let interval_seconds = cfg
            .daemon_interval_seconds
            .or(cfg.rotation_seconds)
            .unwrap_or(300)
            .max(1);
        let interval = Duration::from_secs(interval_seconds);

        let monitors = configured_or_detected_monitors(&cfg)?;
        if monitors.is_empty() {
            wait_for_interval_or_config_change(
                Duration::from_secs(5),
                watched_config_path.as_deref(),
                &mut observed_config_mtime,
            );
            continue;
        }

        for monitor in monitors.iter() {
            if let Some(mut old_child) = monitor_children.remove(monitor) {
                let _ = old_child.kill();
                let _ = old_child.wait();
            }

            let media_dir = media_dir_for_monitor(&cfg, monitor);
            let media = match picker::pick_random_wallpaper(media_dir) {
                Ok(media) => media,
                Err(error) => {
                    eprintln!(
                        "failed to pick wallpaper for monitor '{}' from '{}': {}",
                        monitor,
                        media_dir.display(),
                        error
                    );
                    continue;
                }
            };
            let fit = fit_mode_for_monitor(&cfg, monitor);
            let child = spawn_renderer_child(&media, Some(monitor.as_str()), fps, fit)?;
            monitor_children.insert(monitor.clone(), child);
        }

        monitor_children.retain(|monitor, child| {
            if monitors.contains(monitor) {
                true
            } else {
                let _ = child.kill();
                let _ = child.wait();
                false
            }
        });

        wait_for_interval_or_config_change(
            interval,
            watched_config_path.as_deref(),
            &mut observed_config_mtime,
        );
    }
}

fn wait_for_interval_or_config_change(
    interval: Duration,
    config_path: Option<&Path>,
    observed_mtime: &mut Option<SystemTime>,
) {
    let check_every = Duration::from_secs(1);
    let mut elapsed = Duration::ZERO;

    while elapsed < interval {
        let remaining = interval.saturating_sub(elapsed);
        let sleep_for = remaining.min(check_every);
        thread::sleep(sleep_for);
        elapsed += sleep_for;

        let Some(path) = config_path else {
            continue;
        };

        let current_mtime = config_file_modified_time(path);
        if current_mtime != *observed_mtime {
            *observed_mtime = current_mtime;
            break;
        }
    }
}

fn config_file_modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn resolve_config_watch_path(config_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = config_path {
        return Some(path.to_path_buf());
    }

    let base = env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;

    Some(base.join("papdieo").join("config.toml"))
}

fn acquire_daemon_lock() -> Result<File> {
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(DAEMON_LOCK_PATH)?;

    lock_file
        .try_lock_exclusive()
        .map_err(|_| anyhow!("papdieo daemon is already running"))?;

    Ok(lock_file)
}

fn configured_or_detected_monitors(cfg: &config::Config) -> Result<Vec<String>> {
    if let Some(monitors) = cfg.monitors.as_ref().filter(|m| !m.is_empty()) {
        let cleaned: Vec<String> = monitors
            .iter()
            .map(|m| m.trim())
            .filter(|m| !m.is_empty())
            .map(|m| m.to_string())
            .collect();
        if !cleaned.is_empty() {
            return Ok(cleaned);
        }
    }

    if let Some(map) = cfg.monitor_wallpaper_dirs.as_ref() {
        let mut from_map: Vec<String> = map
            .keys()
            .map(|m| m.trim())
            .filter(|m| !m.is_empty())
            .map(|m| m.to_string())
            .collect();
        from_map.sort();
        from_map.dedup();
        if !from_map.is_empty() {
            return Ok(from_map);
        }
    }

    if let Some(single) = cfg
        .monitor
        .as_ref()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
    {
        return Ok(vec![single]);
    }

    let detected = detect_monitors()?;
    if !detected.is_empty() {
        return Ok(detected);
    }

    Ok(Vec::new())
}

fn detect_monitors() -> Result<Vec<String>> {
    let output = ProcessCommand::new("hyprctl")
        .args(["-j", "monitors"])
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let Some(array) = value.as_array() else {
        return Ok(Vec::new());
    };

    let mut monitors: Vec<String> = array
        .iter()
        .filter_map(|m| m.get("name").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .collect();
    monitors.sort();
    monitors.dedup();
    Ok(monitors)
}

fn media_dir_for_monitor<'a>(cfg: &'a config::Config, monitor: &str) -> &'a Path {
    cfg.monitor_wallpaper_dirs
        .as_ref()
        .and_then(|map| map.get(monitor))
        .map(|path| path.as_path())
        .unwrap_or(cfg.wallpaper_dir.as_path())
}

fn fit_mode_for_monitor(cfg: &config::Config, monitor: &str) -> FitMode {
    cfg.monitor_fit_modes
        .as_ref()
        .and_then(|map| map.get(monitor).copied())
        .or(cfg.fit_mode)
        .unwrap_or(FitMode::Cover)
}

fn spawn_renderer_child(
    path: &Path,
    monitor: Option<&str>,
    fps: u32,
    fit: FitMode,
) -> Result<Child> {
    let exe = std::env::current_exe()?;
    let mut command = ProcessCommand::new(exe);
    command.arg("run-internal").arg(path);
    if let Some(mon) = monitor {
        command.arg("--monitor").arg(mon);
    }
    command
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
        .stderr(Stdio::null());

    Ok(command.spawn()?)
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
