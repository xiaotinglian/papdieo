use anyhow::{anyhow, Context, Result};
use crate::config::FitMode;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use image::{imageops, imageops::FilterType, DynamicImage, RgbaImage};
use memmap2::MmapMut;
use std::{
    fs::File,
    fs::OpenOptions,
    os::fd::AsFd,
    process,
    process::Command,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
    time::Duration,
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1, zwlr_layer_surface_v1,
};

pub fn run_wallpaper(
    path: PathBuf,
    monitor_name: Option<&str>,
    fps: u32,
    fit_mode: FitMode,
) -> Result<()> {
    run_wallpaper_with_stop(path, monitor_name, fps, fit_mode, None)
}

pub fn run_wallpaper_with_stop(
    path: PathBuf,
    monitor_name: Option<&str>,
    fps: u32,
    fit_mode: FitMode,
    stop_signal: Option<&AtomicBool>,
) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("wallpaper does not exist: {}", path.display()));
    }

    let connection = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init::<AppState>(&connection).context("failed to init globals")?;
    let qh = event_queue.handle();

    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("missing wl_compositor")?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("missing wl_shm")?;
    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .context("missing zwlr_layer_shell_v1 (wlr-layer-shell)")?;

    let mut state = AppState::new(path.clone(), monitor_name.map(str::to_string));

    let output_globals: Vec<_> = globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|g| g.interface == "wl_output")
        .collect();

    if output_globals.is_empty() {
        return Err(anyhow!("no wl_output globals found"));
    }

    for g in output_globals {
        let version = g.version.min(4);
        let output =
            globals
                .registry()
                .bind::<wl_output::WlOutput, _, _>(g.name, version, &qh, g.name);
        state.outputs.push(OutputBinding {
            global_name: g.name,
            output,
            name: None,
            description: None,
        });
    }

    event_queue
        .roundtrip(&mut state)
        .context("failed to discover monitor names")?;

    for _ in 0..6 {
        if state.has_resolved_requested_output() || state.all_outputs_have_metadata() {
            break;
        }
        event_queue
            .roundtrip(&mut state)
            .context("failed while waiting for monitor metadata")?;
    }

    let selected_output = state.select_output()?;

    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        Some(&selected_output),
        zwlr_layer_shell_v1::Layer::Background,
        "papdieo".into(),
        &qh,
        (),
    );

    layer_surface.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer_surface.set_size(0, 0);
    layer_surface.set_exclusive_zone(-1);
    surface.commit();

    while !state.configured {
        if stop_signal
            .map(|signal| signal.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            state.exit = true;
            break;
        }
        event_queue
            .blocking_dispatch(&mut state)
            .context("failed during initial Wayland dispatch")?;
    }

    if stop_signal
        .map(|signal| signal.load(Ordering::Relaxed))
        .unwrap_or(false)
    {
        return Ok(());
    }

    let mut frame_renderer = FrameRenderer::new(state.width.max(1), state.height.max(1), &shm, &qh)?;

    if is_video_file(&path) {
        play_video_loop(
            &path,
            &surface,
            &mut frame_renderer,
            &mut event_queue,
            &mut state,
            fps.max(1),
            fit_mode,
            stop_signal,
        )?;
    } else {
        draw_image(&state, &surface, &mut frame_renderer, fit_mode)?;
        while !state.exit {
            if stop_signal
                .map(|signal| signal.load(Ordering::Relaxed))
                .unwrap_or(false)
            {
                state.exit = true;
                break;
            }
            event_queue
                .dispatch_pending(&mut state)
                .context("failed during Wayland event dispatch")?;
            event_queue.flush().ok();
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    drop(layer_surface);
    Ok(())
}

fn draw_image(
    state: &AppState,
    surface: &wl_surface::WlSurface,
    renderer: &mut FrameRenderer,
    fit_mode: FitMode,
) -> Result<()> {
    let width = state.width.max(1);
    let height = state.height.max(1);

    let image = image::open(&state.path)
        .with_context(|| format!("failed to load image: {}", state.path.display()))?;
    let rendered = render_image_fit(&image, width, height, fit_mode);

    draw_image_frame(rendered.as_raw(), surface, renderer)
}

fn play_video_loop(
    path: &Path,
    surface: &wl_surface::WlSurface,
    renderer: &mut FrameRenderer,
    event_queue: &mut EventQueue<AppState>,
    state: &mut AppState,
    fps: u32,
    fit_mode: FitMode,
    stop_signal: Option<&AtomicBool>,
) -> Result<()> {
    gst::init().context("failed to initialize gstreamer")?;

    let width = state.width.max(1);
    let height = state.height.max(1);

    let location = path
        .to_str()
        .ok_or_else(|| anyhow!("video path contains invalid UTF-8"))?
        .replace('\\', "\\\\")
        .replace('"', "\\\"");

    let visibility = HyprlandVisibility::new(state.requested_monitor.as_deref());
    let frame_timeout_ms = (1000 / fps.max(1)).max(4) as u64;

    let descriptions = build_video_pipeline_descriptions(&location, width, height, fps, fit_mode);

    let mut last_error: Option<anyhow::Error> = None;
    for pipeline_desc in descriptions {
        match run_video_pipeline(
            &pipeline_desc,
            width,
            height,
            fit_mode,
            surface,
            renderer,
            event_queue,
            state,
            visibility.as_ref(),
            frame_timeout_ms,
            stop_signal,
        ) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                if state.exit {
                    return Ok(());
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        anyhow!(
            "no video frames decoded; install GStreamer codec plugins (gst-plugins-good, gst-plugins-bad, gst-plugins-ugly, gst-libav)"
        )
    }))
}

fn build_video_pipeline_descriptions(
    location: &str,
    width: u32,
    height: u32,
    fps: u32,
    fit_mode: FitMode,
) -> [String; 4] {
    let scale_stage = video_scale_stage(fit_mode);
    let output_caps = video_output_caps(fit_mode, width, height, fps);

    [
        // NVIDIA fast path
        format!(
            "filesrc location=\"{}\" ! qtdemux ! h264parse ! nvh264dec ! videoconvert{} ! videorate ! {} ! appsink name=sink sync=true max-buffers=1 drop=true",
            location, scale_stage, output_caps
        ),
        // Intel/AMD VA-API decode to lower CPU usage on laptops while preserving contain/fit behavior.
        format!(
            "filesrc location=\"{}\" ! qtdemux ! h264parse ! vaapih264dec ! vaapipostproc ! videoconvert{} ! videorate ! {} ! appsink name=sink sync=true max-buffers=1 drop=true",
            location, scale_stage, output_caps
        ),
        // Generic Vulkan decode
        format!(
            "filesrc location=\"{}\" ! qtdemux ! h264parse ! vulkanh264dec ! videoconvert{} ! videorate ! {} ! appsink name=sink sync=true max-buffers=1 drop=true",
            location, scale_stage, output_caps
        ),
        // Fallback software decode
        format!(
            "filesrc location=\"{}\" ! decodebin ! videoconvert{} ! videorate ! {} ! appsink name=sink sync=true max-buffers=1 drop=true",
            location, scale_stage, output_caps
        ),
    ]
}

fn video_scale_stage(fit_mode: FitMode) -> &'static str {
    match fit_mode {
        FitMode::Center | FitMode::ScaleDown => "",
        FitMode::Fit | FitMode::Contain => " ! videoscale add-borders=true",
        _ => " ! videoscale",
    }
}

fn video_output_caps(fit_mode: FitMode, width: u32, height: u32, fps: u32) -> String {
    match fit_mode {
        FitMode::Center | FitMode::ScaleDown => {
            format!("video/x-raw,format=BGRx,framerate={}/1", fps)
        }
        _ => format!(
            "video/x-raw,format=BGRx,width={},height={},framerate={}/1",
            width, height, fps
        ),
    }
}

fn render_image_fit(
    image: &DynamicImage,
    out_w: u32,
    out_h: u32,
    fit_mode: FitMode,
) -> RgbaImage {
    render_rgba_fit(&image.to_rgba8(), out_w, out_h, fit_mode)
}

fn render_rgba_fit(image: &RgbaImage, out_w: u32, out_h: u32, fit_mode: FitMode) -> RgbaImage {
    match fit_mode {
        FitMode::Stretch => imageops::resize(image, out_w, out_h, FilterType::Lanczos3),
        FitMode::Fit | FitMode::Contain => render_contained_rgba(image, out_w, out_h, true),
        FitMode::Center => render_centered_rgba(image, out_w, out_h),
        FitMode::ScaleDown => render_contained_rgba(image, out_w, out_h, false),
        FitMode::Fill | FitMode::Cover => {
            let scale = f64::max(
                out_w as f64 / image.width() as f64,
                out_h as f64 / image.height() as f64,
            );
            let rw = (image.width() as f64 * scale).round().max(out_w as f64) as u32;
            let rh = (image.height() as f64 * scale).round().max(out_h as f64) as u32;
            let resized = imageops::resize(image, rw, rh, FilterType::Lanczos3);
            let x = (rw.saturating_sub(out_w)) / 2;
            let y = (rh.saturating_sub(out_h)) / 2;
            imageops::crop_imm(&resized, x, y, out_w, out_h).to_image()
        }
    }
}

fn render_contained_rgba(
    image: &RgbaImage,
    out_w: u32,
    out_h: u32,
    allow_upscale: bool,
) -> RgbaImage {
    let scale = f64::min(
        out_w as f64 / image.width() as f64,
        out_h as f64 / image.height() as f64,
    );
    let scale = if allow_upscale { scale } else { scale.min(1.0) };
    let rw = (image.width() as f64 * scale).round().max(1.0) as u32;
    let rh = (image.height() as f64 * scale).round().max(1.0) as u32;
    let resized = if rw == image.width() && rh == image.height() {
        image.clone()
    } else {
        imageops::resize(image, rw, rh, FilterType::Lanczos3)
    };

    let mut canvas = RgbaImage::new(out_w, out_h);
    let x = ((out_w as i64 - resized.width() as i64) / 2).max(0) as u32;
    let y = ((out_h as i64 - resized.height() as i64) / 2).max(0) as u32;
    imageops::overlay(&mut canvas, &resized, x as i64, y as i64);
    canvas
}

fn render_centered_rgba(image: &RgbaImage, out_w: u32, out_h: u32) -> RgbaImage {
    let crop_w = image.width().min(out_w);
    let crop_h = image.height().min(out_h);
    let src_x = image.width().saturating_sub(crop_w) / 2;
    let src_y = image.height().saturating_sub(crop_h) / 2;
    let dst_x = out_w.saturating_sub(crop_w) / 2;
    let dst_y = out_h.saturating_sub(crop_h) / 2;
    let cropped = imageops::crop_imm(image, src_x, src_y, crop_w, crop_h).to_image();
    let mut canvas = RgbaImage::new(out_w, out_h);
    imageops::overlay(&mut canvas, &cropped, dst_x as i64, dst_y as i64);
    canvas
}

fn run_video_pipeline(
    pipeline_desc: &str,
    width: u32,
    height: u32,
    fit_mode: FitMode,
    surface: &wl_surface::WlSurface,
    renderer: &mut FrameRenderer,
    event_queue: &mut EventQueue<AppState>,
    state: &mut AppState,
    visibility: Option<&HyprlandVisibility>,
    frame_timeout_ms: u64,
    stop_signal: Option<&AtomicBool>,
) -> Result<()> {
    let pipeline = gst::parse::launch(pipeline_desc)
        .context("failed to build gstreamer pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("gstreamer element is not a pipeline"))?;

    let sink = pipeline
        .by_name("sink")
        .ok_or_else(|| anyhow!("missing appsink in gstreamer pipeline"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("sink is not an appsink"))?;

    let bus = pipeline
        .bus()
        .ok_or_else(|| anyhow!("gstreamer pipeline has no bus"))?;

    pipeline
        .set_state(gst::State::Playing)
        .context("failed to start video pipeline")?;

    let Some(initial_sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(2)) else {
        pipeline.set_state(gst::State::Null).ok();
        return Err(anyhow!("no initial video frame from pipeline"));
    };

    if !write_sample_frame(
        &initial_sample,
        surface,
        renderer,
        width as usize,
        height as usize,
        fit_mode,
    )? {
        pipeline.set_state(gst::State::Null).ok();
        return Err(anyhow!("no free Wayland frame buffer for initial video frame"));
    }

    let mut is_paused = false;
    let mut last_visibility_refresh = Instant::now();

    while !state.exit {
        if stop_signal
            .map(|signal| signal.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            state.exit = true;
            break;
        }

        if let Some(v) = visibility {
            if last_visibility_refresh.elapsed() >= Duration::from_millis(500) {
                v.refresh_now();
                last_visibility_refresh = Instant::now();
            }
        }

        let should_render = visibility.map(|v| v.should_render()).unwrap_or(true);
        if should_render && is_paused {
            pipeline.set_state(gst::State::Playing).ok();
            is_paused = false;
        } else if !should_render && !is_paused {
            pipeline.set_state(gst::State::Paused).ok();
            is_paused = true;
        }

        let mut waited_for_release = false;

        if should_render {
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(frame_timeout_ms)) {
                let wrote_frame = write_sample_frame(
                    &sample,
                    surface,
                    renderer,
                    width as usize,
                    height as usize,
                    fit_mode,
                )?;

                if !wrote_frame {
                    // All shm buffers are currently held by the compositor.
                    // Block until at least one release event arrives.
                    event_queue
                        .blocking_dispatch(state)
                        .context("failed while waiting for Wayland frame release")?;
                    waited_for_release = true;
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(120));
        }

        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            match msg.type_() {
                gst::MessageType::Error => {
                    pipeline.set_state(gst::State::Null).ok();
                    return Err(anyhow!("video pipeline error"));
                }
                gst::MessageType::Eos => {
                    let _ = pipeline.seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                        gst::ClockTime::from_seconds(0),
                    );
                }
                _ => {}
            }
        }

        if !waited_for_release {
            event_queue
                .dispatch_pending(state)
                .context("failed dispatching Wayland events")?;
        }
        event_queue.flush().ok();
    }

    pipeline.set_state(gst::State::Null).ok();
    Ok(())
}

fn write_sample_frame(
    sample: &gst::Sample,
    surface: &wl_surface::WlSurface,
    renderer: &mut FrameRenderer,
    width: usize,
    height: usize,
    fit_mode: FitMode,
) -> Result<bool> {
    let Some(slot) = renderer.acquire_slot() else {
        return Ok(false);
    };

    if let Err(error) = renderer.write_sample_bgrx(slot, sample, width, height, fit_mode) {
        renderer.release_slot(slot);
        return Err(error);
    }

    surface.attach(Some(renderer.buffer(slot)), 0, 0);
    surface.damage_buffer(0, 0, renderer.width as i32, renderer.height as i32);
    surface.commit();

    Ok(true)
}

fn draw_image_frame(
    rgba_bytes: &[u8],
    surface: &wl_surface::WlSurface,
    renderer: &mut FrameRenderer,
) -> Result<()> {
    let slot = renderer
        .acquire_slot()
        .ok_or_else(|| anyhow!("no free Wayland frame buffer for image frame"))?;

    if let Err(error) = renderer.write_rgba_image_frame(slot, rgba_bytes) {
        renderer.release_slot(slot);
        return Err(error);
    }

    surface.attach(Some(renderer.buffer(slot)), 0, 0);
    surface.damage_buffer(0, 0, renderer.width as i32, renderer.height as i32);
    surface.commit();

    Ok(())
}

struct FrameSlot {
    frame_size: usize,
    mmap: MmapMut,
    _file: File,
    _pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    in_use: Arc<AtomicBool>,
}

struct FrameRenderer {
    width: u32,
    height: u32,
    slots: Vec<FrameSlot>,
    next_slot: usize,
}

impl FrameRenderer {
    fn new(
        width: u32,
        height: u32,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<AppState>,
    ) -> Result<Self> {
        let stride = (width * 4) as i32;
        let frame_size = (height as i32 * stride) as usize;
        let mut slots = Vec::with_capacity(2);

        for index in 0..2 {
            let buffer_path = std::env::temp_dir()
                .join(format!("papdieo-buffer-{}-{}", process::id(), index));

            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&buffer_path)
                .context("failed to create shared memory buffer file")?;
            file.set_len(frame_size as u64)?;
            let _ = std::fs::remove_file(&buffer_path);

            let mmap =
                unsafe { MmapMut::map_mut(&file) }.context("failed to map shared memory")?;

            let in_use = Arc::new(AtomicBool::new(false));
            let pool = shm.create_pool(file.as_fd(), frame_size as i32, qh, ());
            let buffer = pool.create_buffer(
                0,
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Xrgb8888,
                qh,
                in_use.clone(),
            );

            slots.push(FrameSlot {
                frame_size,
                mmap,
                _file: file,
                _pool: pool,
                buffer,
                in_use,
            });
        }

        Ok(Self {
            width,
            height,
            slots,
            next_slot: 0,
        })
    }

    fn acquire_slot(&mut self) -> Option<usize> {
        for offset in 0..self.slots.len() {
            let idx = (self.next_slot + offset) % self.slots.len();
            let slot = &self.slots[idx];
            if !slot.in_use.load(Ordering::Acquire) {
                slot.in_use.store(true, Ordering::Release);
                self.next_slot = (idx + 1) % self.slots.len();
                return Some(idx);
            }
        }

        None
    }

    fn release_slot(&self, slot_idx: usize) {
        if let Some(slot) = self.slots.get(slot_idx) {
            slot.in_use.store(false, Ordering::Release);
        }
    }

    fn buffer(&self, slot_idx: usize) -> &wl_buffer::WlBuffer {
        &self.slots[slot_idx].buffer
    }

    fn write_sample_bgrx(
        &mut self,
        slot_idx: usize,
        sample: &gst::Sample,
        width: usize,
        height: usize,
        fit_mode: FitMode,
    ) -> Result<()> {
        let buffer = sample
            .buffer()
            .ok_or_else(|| anyhow!("video sample missing buffer"))?;
        let map = buffer
            .map_readable()
            .map_err(|_| anyhow!("failed to map video buffer"))?;

        let caps = sample
            .caps()
            .ok_or_else(|| anyhow!("video sample missing caps"))?;
        let info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|_| anyhow!("failed to parse video caps"))?;

        let stride = info.stride()[0] as usize;
        let src = map.as_slice();
        let row_bytes = width * 4;

        if info.width() != width as u32
            || info.height() != height as u32
            || matches!(fit_mode, FitMode::Center | FitMode::ScaleDown)
        {
            let rgba = rgba_from_bgrx_frame(src, stride, info.width(), info.height())?;
            let rendered = render_rgba_fit(&rgba, width as u32, height as u32, fit_mode);
            return self.write_rgba_image_frame(slot_idx, rendered.as_raw());
        }

        if row_bytes * height > self.slots[slot_idx].frame_size {
            return Err(anyhow!("video frame larger than renderer buffer"));
        }

        for row in 0..height {
            let src_start = row * stride;
            let dst_start = row * row_bytes;
            let src_end = src_start + row_bytes;
            let dst_end = dst_start + row_bytes;
            if src_end > src.len() {
                return Err(anyhow!("video frame stride exceeds buffer"));
            }
            self.slots[slot_idx].mmap[dst_start..dst_end].copy_from_slice(&src[src_start..src_end]);
        }

        Ok(())
    }

    fn write_rgba_image_frame(&mut self, slot_idx: usize, rgba: &[u8]) -> Result<()> {
        if rgba.len() > self.slots[slot_idx].frame_size {
            return Err(anyhow!("image frame is larger than renderer buffer"));
        }

        for (dst, px) in self.slots[slot_idx].mmap[..rgba.len()]
            .chunks_exact_mut(4)
            .zip(rgba.chunks_exact(4))
        {
            dst[0] = px[2];
            dst[1] = px[1];
            dst[2] = px[0];
            dst[3] = 255;
        }
        Ok(())
    }
}

fn rgba_from_bgrx_frame(src: &[u8], stride: usize, width: u32, height: u32) -> Result<RgbaImage> {
    let row_bytes = width as usize * 4;
    let mut rgba = RgbaImage::new(width, height);

    for row in 0..height as usize {
        let src_start = row * stride;
        let src_end = src_start + row_bytes;
        if src_end > src.len() {
            return Err(anyhow!("video frame stride exceeds buffer"));
        }

        for (column, px) in src[src_start..src_end].chunks_exact(4).enumerate() {
            let dst = rgba.get_pixel_mut(column as u32, row as u32);
            dst[0] = px[2];
            dst[1] = px[1];
            dst[2] = px[0];
            dst[3] = 255;
        }
    }

    Ok(rgba)
}

fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp4" | "mkv" | "webm" | "mov" | "avi"
            )
        })
        .unwrap_or(false)
}

struct HyprlandVisibility {
    should_render: Arc<AtomicBool>,
    target_monitor_id: Option<i64>,
}

impl HyprlandVisibility {
    fn new(target_monitor_name: Option<&str>) -> Option<Self> {
        let target_monitor_id = resolve_monitor_id(target_monitor_name);
        let initial_should_render = query_should_render(target_monitor_id).unwrap_or(true);

        Some(Self {
            should_render: Arc::new(AtomicBool::new(initial_should_render)),
            target_monitor_id,
        })
    }

    fn should_render(&self) -> bool {
        self.should_render.load(Ordering::Relaxed)
    }

    fn refresh_now(&self) {
        if let Some(should_render) = query_should_render(self.target_monitor_id) {
            self.should_render.store(should_render, Ordering::Relaxed);
        }
    }
}

fn resolve_monitor_id(target_monitor_name: Option<&str>) -> Option<i64> {
    let name = target_monitor_name?;
    let output = Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    value
        .as_array()?
        .iter()
        .find(|m| m.get("name").and_then(|v| v.as_str()) == Some(name))
        .and_then(|m| m.get("id").and_then(|v| v.as_i64()))
}

fn query_should_render(target_monitor_id: Option<i64>) -> Option<bool> {
    let active_workspace_id = active_workspace_id(target_monitor_id)?;

    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let clients = value.as_array()?;

    let has_window_on_active_workspace = clients.iter().any(|client| {
        let mapped = client
            .get("mapped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let hidden = client
            .get("hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !mapped || hidden {
            return false;
        }

        let workspace_id = client
            .get("workspace")
            .and_then(|ws| ws.get("id"))
            .and_then(|id| id.as_i64());

        workspace_id == Some(active_workspace_id)
    });

    Some(!has_window_on_active_workspace)
}

fn active_workspace_id(target_monitor_id: Option<i64>) -> Option<i64> {
    let output = Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let monitors = value.as_array()?;

    let monitor = if let Some(target_id) = target_monitor_id {
        monitors
            .iter()
            .find(|m| m.get("id").and_then(|v| v.as_i64()) == Some(target_id))
    } else {
        monitors
            .iter()
            .find(|m| m.get("focused").and_then(|v| v.as_bool()) == Some(true))
            .or_else(|| monitors.first())
    }?;

    monitor
        .get("activeWorkspace")
        .and_then(|ws| ws.get("id"))
        .and_then(|id| id.as_i64())
}

struct AppState {
    path: PathBuf,
    requested_monitor: Option<String>,
    outputs: Vec<OutputBinding>,
    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
}

impl AppState {
    fn new(path: PathBuf, requested_monitor: Option<String>) -> Self {
        Self {
            path,
            requested_monitor: requested_monitor
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty()),
            outputs: Vec::new(),
            width: 1920,
            height: 1080,
            configured: false,
            exit: false,
        }
    }

    fn has_resolved_requested_output(&self) -> bool {
        let Some(requested) = self.requested_monitor.as_deref() else {
            return true;
        };
        self.outputs.iter().any(|out| output_matches_monitor(out, requested))
    }

    fn all_outputs_have_metadata(&self) -> bool {
        self.outputs
            .iter()
            .all(|out| out.name.is_some() || out.description.is_some())
    }

    fn select_output(&self) -> Result<wl_output::WlOutput> {
        if let Some(requested) = &self.requested_monitor {
            if let Some(found) = self
                .outputs
                .iter()
                .find(|out| output_matches_monitor(out, requested))
            {
                return Ok(found.output.clone());
            }

            let available: Vec<String> = self
                .outputs
                .iter()
                .filter_map(|out| {
                    if let Some(name) = &out.name {
                        Some(name.clone())
                    } else {
                        out.description
                            .as_ref()
                            .map(|desc| format!("{} (description)", desc))
                    }
                })
                .collect();
            return Err(anyhow!(
                "requested monitor '{}' was not found (available: {})",
                requested,
                if available.is_empty() {
                    "unknown".to_string()
                } else {
                    available.join(", ")
                }
            ));
        }

        self.outputs
            .first()
            .map(|out| out.output.clone())
            .ok_or_else(|| anyhow!("no outputs available"))
    }
}

struct OutputBinding {
    global_name: u32,
    output: wl_output::WlOutput,
    name: Option<String>,
    description: Option<String>,
}

fn output_matches_monitor(output: &OutputBinding, requested: &str) -> bool {
    let requested = requested.trim();

    if let Some(name) = output.name.as_deref() {
        if name == requested || name.eq_ignore_ascii_case(requested) {
            return true;
        }
    }

    if let Some(description) = output.description.as_deref() {
        let requested_lower = requested.to_ascii_lowercase();
        let desc_lower = description.to_ascii_lowercase();
        if desc_lower == requested_lower || desc_lower.contains(&requested_lower) {
            return true;
        }
    }

    false
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, u32> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Name { name } => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.global_name == *data) {
                    output.name = Some(name);
                }
            }
            wl_output::Event::Description { description } => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.global_name == *data) {
                    output.description = Some(description);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &Arc<AtomicBool>,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            data.store(false, Ordering::Release);
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                proxy.ack_configure(serial);
                if width > 0 {
                    state.width = width;
                }
                if height > 0 {
                    state.height = height;
                }
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.exit = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_video_pipeline_descriptions, render_rgba_fit};
    use crate::config::FitMode;
    use image::RgbaImage;

    #[test]
    fn contain_enables_borders_for_all_video_pipelines() {
        let descriptions =
            build_video_pipeline_descriptions("/tmp/demo.mp4", 1920, 1080, 60, FitMode::Contain);

        assert!(descriptions
            .iter()
            .all(|pipeline| pipeline.contains("videoscale add-borders=true")));
    }

    #[test]
    fn cover_does_not_enable_video_borders() {
        let descriptions =
            build_video_pipeline_descriptions("/tmp/demo.mp4", 1920, 1080, 60, FitMode::Cover);

        assert!(descriptions
            .iter()
            .all(|pipeline| !pipeline.contains("add-borders=true")));
    }

    #[test]
    fn center_video_pipelines_keep_source_dimensions() {
        let descriptions =
            build_video_pipeline_descriptions("/tmp/demo.mp4", 1920, 1080, 60, FitMode::Center);

        assert!(descriptions.iter().all(|pipeline| !pipeline.contains("videoscale")));
        assert!(descriptions
            .iter()
            .all(|pipeline| !pipeline.contains("width=1920") && !pipeline.contains("height=1080")));
    }

    #[test]
    fn scale_down_does_not_upscale_smaller_images() {
        let mut image = RgbaImage::new(1, 1);
        image.put_pixel(0, 0, image::Rgba([255, 255, 255, 255]));

        let rendered = render_rgba_fit(&image, 3, 3, FitMode::ScaleDown);

        assert_eq!(rendered.get_pixel(1, 1).0, [255, 255, 255, 255]);
        assert_eq!(rendered.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(rendered.get_pixel(2, 2).0, [0, 0, 0, 0]);
    }
}
