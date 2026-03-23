mod cli;
mod config;
mod picker;
mod wallpaper;

use anyhow::{anyhow, Result};
use clap::Parser;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::File,
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, SystemTime},
};

use cli::{Command, PapdieoArgs};
use config::FitMode;

const DAEMON_PID_PATH: &str = "/tmp/papdieo-daemon.pid";
const DAEMON_LOG_PATH: &str = "/tmp/papdieo-daemon.log";
const DAEMON_LOCK_PATH: &str = "/tmp/papdieo-daemon.lock";
const DAEMON_STARTUP_RETRY_SECONDS: u64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MonitorAssignment {
    monitor: String,
    path: PathBuf,
    fit: FitMode,
}

fn main() {
    let args = PapdieoArgs::parse();
    if let Err(error) = run(args) {
        if is_broken_pipe_error(&error) {
            return;
        }

        eprintln!("{:#}", error);
        std::process::exit(1);
    }
}

fn is_broken_pipe_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .map(|io_error| io_error.kind() == io::ErrorKind::BrokenPipe)
            .unwrap_or_else(|| cause.to_string().to_ascii_lowercase().contains("broken pipe"))
    })
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
        Some(Command::Restart) => restart_daemon_service(args.config.as_deref()),
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
            assignments,
            monitor,
            fps,
            fit,
        }) => {
            let resolved_fps = fps.unwrap_or(default_fps);
            if let Some(assignments_json) = assignments {
                let assignments: Vec<MonitorAssignment> = serde_json::from_str(&assignments_json)
                    .map_err(|e| anyhow!("invalid internal assignments payload: {}", e))?;
                return run_wallpaper_assignments(assignments, resolved_fps);
            }

            let path = path.ok_or_else(|| anyhow!("missing wallpaper path for run-internal"))?;
            wallpaper::run_wallpaper(
                path,
                monitor.as_deref(),
                resolved_fps,
                fit.unwrap_or(default_fit),
            )
        }
        Some(Command::__DaemonInternal) => run_daemon_loop(args.config.as_deref()),
    }
}

fn run_wallpaper_assignments(assignments: Vec<MonitorAssignment>, fps: u32) -> Result<()> {
    run_wallpaper_assignments_cancellable(assignments, fps, None)
}

fn run_wallpaper_assignments_cancellable(
    assignments: Vec<MonitorAssignment>,
    fps: u32,
    stop_signal: Option<Arc<AtomicBool>>,
) -> Result<()> {
    if assignments.is_empty() {
        return Err(anyhow!("no monitor assignments provided"));
    }

    let mut workers = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let monitor = assignment.monitor.clone();
        let worker_stop = stop_signal.clone();
        workers.push((monitor, thread::spawn(move || {
            wallpaper::run_wallpaper_with_stop(
                assignment.path,
                Some(assignment.monitor.as_str()),
                fps,
                assignment.fit,
                worker_stop.as_deref(),
            )
        })));
    }

    // Each worker should keep running while wallpaper is active; if one exits,
    // fail fast so daemon can retry the full assignment set.
    loop {
        if stop_signal
            .as_ref()
            .map(|signal| signal.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            break;
        }

        let mut idx = 0;
        while idx < workers.len() {
            if workers[idx].1.is_finished() {
                let (monitor, worker) = workers.remove(idx);
                let result = worker.join().map_err(|_| {
                    anyhow!("renderer thread panicked for monitor '{}'", monitor)
                })?;
                match result {
                    Ok(()) => {
                        return Err(anyhow!(
                            "renderer thread exited unexpectedly for monitor '{}'",
                            monitor
                        ));
                    }
                    Err(error) => {
                        return Err(anyhow!(
                            "renderer thread failed for monitor '{}': {}",
                            monitor,
                            error
                        ));
                    }
                }
            }
            idx += 1;
        }

        thread::sleep(Duration::from_millis(250));
    }

    for (monitor, worker) in workers {
        let result = worker
            .join()
            .map_err(|_| anyhow!("renderer thread panicked for monitor '{}'", monitor))?;
        if let Err(error) = result {
            return Err(anyhow!(
                "renderer thread failed for monitor '{}': {}",
                monitor,
                error
            ));
        }
    }

    Ok(())
}

fn restart_daemon_service(config_path: Option<&Path>) -> Result<()> {
    stop_daemon_service()?;
    start_daemon_service(config_path)
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
        .truncate(true)
        .write(true)
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

fn stop_daemon_service() -> Result<()> {
    let pid_path = Path::new(DAEMON_PID_PATH);
    let Ok(content) = std::fs::read_to_string(pid_path) else {
        cleanup_renderer_processes();
        println!("papdieo daemon not running");
        return Ok(());
    };

    let pid = match content.trim().parse::<u32>() {
        Ok(pid) => pid,
        Err(_) => {
            let _ = std::fs::remove_file(pid_path);
            cleanup_renderer_processes();
            return Err(anyhow!("invalid daemon pid file: {}", DAEMON_PID_PATH));
        }
    };

    if PathBuf::from(format!("/proc/{pid}")).exists() {
        let status = ProcessCommand::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()?;
        if !status.success() {
            return Err(anyhow!("failed to stop daemon process {}", pid));
        }

        let mut exited = false;
        for _ in 0..20 {
            if !PathBuf::from(format!("/proc/{pid}")).exists() {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        if !exited {
            let _ = ProcessCommand::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            for _ in 0..10 {
                if !PathBuf::from(format!("/proc/{pid}")).exists() {
                    exited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }

        if !exited {
            return Err(anyhow!("timed out while stopping daemon process {}", pid));
        }
    }

    let _ = std::fs::remove_file(pid_path);
    cleanup_renderer_processes();
    println!("Stopped papdieo daemon");
    Ok(())
}

fn cleanup_renderer_processes() {
    let _ = ProcessCommand::new("pkill")
        .args(["-f", "papdieo run-internal"])
        .status();
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

    let watched_config_path = resolve_config_watch_path(config_path);
    let mut observed_config_mtime = watched_config_path
        .as_ref()
        .and_then(|path| config_file_modified_time(path));

    loop {
        let cfg = config::Config::load_or_default(config_path)?;
        let fps = cfg.video_fps.unwrap_or(60);
        let configured_interval_seconds = cfg
            .daemon_interval_seconds
            .or(cfg.rotation_seconds)
            .unwrap_or(300)
            .max(1);
        let interval = Duration::from_secs(configured_interval_seconds);

        let monitors = configured_or_detected_monitors(&cfg)?;
        warn_unknown_monitor_map_keys(&cfg, &monitors);
        if monitors.is_empty() {
            wait_for_interval_or_config_change(
                Duration::from_secs(5),
                watched_config_path.as_deref(),
                &mut observed_config_mtime,
            );
            continue;
        }

        let mut assignments = Vec::new();

        for monitor in monitors.iter() {
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
            assignments.push(MonitorAssignment {
                monitor: monitor.clone(),
                path: media,
                fit,
            });
        }

        let mut launched_any_renderer = false;

        if !assignments.is_empty() {
            launched_any_renderer = true;
            let stop_signal = Arc::new(AtomicBool::new(false));
            let worker_stop_signal = Arc::clone(&stop_signal);
            let worker = thread::spawn(move || {
                run_wallpaper_assignments_cancellable(assignments, fps, Some(worker_stop_signal))
            });

            let mut elapsed = Duration::ZERO;
            let check_every = Duration::from_secs(1);
            while elapsed < interval {
                if worker.is_finished() {
                    break;
                }

                let remaining = interval.saturating_sub(elapsed);
                let sleep_for = remaining.min(check_every);
                thread::sleep(sleep_for);
                elapsed += sleep_for;

                let Some(path) = watched_config_path.as_deref() else {
                    continue;
                };

                let current_mtime = config_file_modified_time(path);
                if current_mtime != observed_config_mtime {
                    observed_config_mtime = current_mtime;
                    break;
                }
            }

            stop_signal.store(true, Ordering::Relaxed);

            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    if !is_broken_pipe_error(&error) {
                        eprintln!("failed to run renderer assignments in daemon process: {}", error);
                    }
                }
                Err(_) => {
                    eprintln!("renderer assignment worker panicked");
                }
            }
        }

        if !launched_any_renderer {
            wait_for_interval_or_config_change(
                Duration::from_secs(DAEMON_STARTUP_RETRY_SECONDS),
                watched_config_path.as_deref(),
                &mut observed_config_mtime,
            );
        }
    }
}

fn warn_unknown_monitor_map_keys(cfg: &config::Config, active_monitors: &[String]) {
    let active: std::collections::HashSet<&str> = active_monitors.iter().map(String::as_str).collect();

    if let Some(map) = cfg.monitor_wallpaper_dirs.as_ref() {
        let unknown: Vec<&str> = map
            .keys()
            .map(String::as_str)
            .filter(|k| !active.contains(k.trim()))
            .collect();
        if !unknown.is_empty() {
            eprintln!(
                "warning: monitor_wallpaper_dirs has unknown monitor keys: {}",
                unknown.join(", ")
            );
        }
    }

    if let Some(map) = cfg.monitor_fit_modes.as_ref() {
        let unknown: Vec<&str> = map
            .keys()
            .map(String::as_str)
            .filter(|k| !active.contains(k.trim()))
            .collect();
        if !unknown.is_empty() {
            eprintln!(
                "warning: monitor_fit_modes has unknown monitor keys: {}",
                unknown.join(", ")
            );
        }
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
            let detected = detect_monitors()?;
            if !detected.is_empty() {
                let detected_set: std::collections::HashSet<&str> =
                    detected.iter().map(String::as_str).collect();
                let mut intersection: Vec<String> = from_map
                    .iter()
                    .filter(|m| detected_set.contains(m.as_str()))
                    .cloned()
                    .collect();

                if intersection.is_empty() {
                    eprintln!(
                        "warning: monitor_wallpaper_dirs keys do not match detected monitors; using detected monitors instead"
                    );
                    return Ok(detected);
                }

                intersection.sort();
                intersection.dedup();
                return Ok(intersection);
            }

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
