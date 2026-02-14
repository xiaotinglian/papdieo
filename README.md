# papdieo

A Rust-based, Hyprland-compatible wallpaper tool.

## Dependencies

### Required

- Rust toolchain (`rustc`, `cargo`) — Rust 1.75+ recommended
- Wayland session (Hyprland)
- GStreamer runtime + plugins for video decode

### Arch Linux packages

Install everything needed to build and run:

```bash
sudo pacman -S --needed \
	rust \
	base-devel \
	pkgconf \
	wayland \
	gstreamer \
	gst-plugins-base \
	gst-plugins-good \
	gst-plugins-bad \
	gst-plugins-ugly \
	gst-libav
```

### Optional (for better NVIDIA video decode path)

- `nvidia-utils`
- `vulkan-icd-loader`

If these are available, `papdieo` can use hardware-accelerated decode (`nvh264dec` / `vulkanh264dec`) before fallback.

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
papdieo random

# Random pick from a specific folder
papdieo random --dir /path/to/media

# Set explicit wallpaper
papdieo set /path/to/wallpaper.png

# Set looping video wallpaper
papdieo set /path/to/wallpaper.mp4 --monitor DP-4 --detach

# Set video wallpaper at explicit FPS
papdieo set /path/to/wallpaper.mp4 --monitor DP-4 --fps 60 --detach

# Set on a specific monitor
papdieo set /path/to/wallpaper.png --monitor DP-4

# Cycle to next wallpaper
papdieo next

# Auto-rotate random media every configured interval
papdieo rotate

# Auto-rotate random media from a specific folder every 120s
papdieo rotate --dir /path/to/media --interval 120

# List discovered wallpapers
papdieo list

# Run renderer detached (background)
papdieo set /path/to/wallpaper.png --detach
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
~/.config/papdieo/config.toml
```

Use it with:

```bash
papdieo --config /path/to/papdieo.toml random
```

## Notes

- Run this inside a Wayland/Hyprland session (`WAYLAND_DISPLAY` must be set).
- This tool renders wallpaper directly via `wlr-layer-shell` protocol.
- Video playback requires GStreamer codec plugins (`gst-plugins-good`, `gst-plugins-bad`, `gst-plugins-ugly`, `gst-libav`).
- On Hyprland, video rendering pauses automatically when an active window is present and resumes on desktop visibility.
