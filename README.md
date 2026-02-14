# hyprwall

A Rust-based, Hyprland-compatible wallpaper tool.

## Features

- Native Wayland wallpaper renderer (no `hyprpaper`, no `hyprctl`, no external wallpaper daemon)
- Native video wallpaper support (`.mp4`, `.mkv`, `.webm`, `.mov`, `.avi`)
- NVIDIA-first hardware decode path (with fallback)
- Default video target FPS: `60`
- Set a specific wallpaper file
- Pick a random wallpaper from a directory
- Cycle to the next wallpaper
- List available wallpapers
- Optional detached/background launch mode

## Build

```bash
cargo build --release
```

## Usage

```bash
# Random wallpaper from default ~/Pictures/Wallpapers
cargo run -- random

# Random pick from a specific folder
cargo run -- random --dir /path/to/media

# Set explicit wallpaper
cargo run -- set /path/to/wallpaper.png

# Set looping video wallpaper
cargo run -- set /path/to/wallpaper.mp4 --monitor DP-4 --detach

# Set video wallpaper at explicit FPS
cargo run -- set /path/to/wallpaper.mp4 --monitor DP-4 --fps 60 --detach

# Set on a specific monitor
cargo run -- set /path/to/wallpaper.png --monitor DP-4

# Cycle to next wallpaper
cargo run -- next

# Auto-rotate random media every configured interval
cargo run -- rotate

# Auto-rotate random media from a specific folder every 120s
cargo run -- rotate --dir /path/to/media --interval 120

# List discovered wallpapers
cargo run -- list

# Run renderer detached (background)
cargo run -- set /path/to/wallpaper.png --detach
```

## Config

Optional TOML config:

```toml
wallpaper_dir = "/home/youruser/Pictures/Wallpapers"
monitor = "DP-4"
video_fps = 60
rotation_seconds = 300
fit_mode = "cover"
```

Supported `fit_mode` values:

```text
stretch | fill | cover | fit | contain
```

Default auto-load path (no `--config` needed):

```text
~/.config/hyprwall/config.toml
```

Use it with:

```bash
cargo run -- --config /path/to/hyprwall.toml random
```

## Notes

- Run this inside a Wayland/Hyprland session (`WAYLAND_DISPLAY` must be set).
- This tool renders wallpaper directly via `wlr-layer-shell` protocol.
- Video playback requires GStreamer codec plugins (`gst-plugins-good`, `gst-plugins-bad`, `gst-plugins-ugly`, `gst-libav`).
- On Hyprland, video rendering pauses automatically when an active window is present and resumes on desktop visibility.
