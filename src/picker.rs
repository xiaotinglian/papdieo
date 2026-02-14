use anyhow::{anyhow, Result};
use rand::seq::SliceRandom;
use std::{
    fs,
    path::{Path, PathBuf},
};

const STATE_FILE: &str = "/tmp/hyprwall_state";

pub fn list_wallpapers(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Err(anyhow!("wallpaper directory does not exist: {}", dir.display()));
    }

    let mut images = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && is_supported_media(&path) {
            images.push(path);
        }
    }

    images.sort();
    if images.is_empty() {
        return Err(anyhow!("no wallpapers found in {}", dir.display()));
    }

    Ok(images)
}

pub fn pick_random_wallpaper(dir: &Path) -> Result<PathBuf> {
    let images = list_wallpapers(dir)?;
    let mut rng = rand::thread_rng();
    let mut selected = images
        .choose(&mut rng)
        .cloned()
        .ok_or_else(|| anyhow!("no wallpapers available"))?;

    if images.len() > 1 {
        if let Ok(last_path) = fs::read_to_string(STATE_FILE) {
            let last = last_path.trim();
            if selected.to_string_lossy() == last {
                let alternatives: Vec<_> = images
                    .iter()
                    .filter(|p| p.to_string_lossy() != last)
                    .cloned()
                    .collect();
                if let Some(next) = alternatives.choose(&mut rng).cloned() {
                    selected = next;
                }
            }
        }
    }

    let _ = fs::write(STATE_FILE, selected.to_string_lossy().as_bytes());
    Ok(selected)
}

pub fn pick_next_wallpaper(dir: &Path) -> Result<PathBuf> {
    let images = list_wallpapers(dir)?;
    let last = fs::read_to_string(STATE_FILE).ok();

    let next_index = match last {
        Some(last_path) => images
            .iter()
            .position(|p| p.to_string_lossy() == last_path.trim())
            .map(|i| (i + 1) % images.len())
            .unwrap_or(0),
        None => 0,
    };

    let next = images[next_index].clone();
    let _ = fs::write(STATE_FILE, next.to_string_lossy().as_bytes());
    Ok(next)
}

fn is_supported_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "jpg"
                    | "jpeg"
                    | "png"
                    | "webp"
                    | "mp4"
                    | "mkv"
                    | "webm"
                    | "mov"
                    | "avi"
            )
        })
        .unwrap_or(false)
}
