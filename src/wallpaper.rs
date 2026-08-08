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
        atomic::{AtomicBool, AtomicU64, Ordering},
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

static BUFFER_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    let descriptions = filter_available_video_pipelines(descriptions);

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
) -> Vec<String> {
    let fit_stage = video_fit_stage(fit_mode, width, height);
    let output_caps = video_output_caps(fit_mode, width, height, fps);
    let decoder_stages = [
        "qtdemux ! h264parse ! nvh264dec",
        "qtdemux ! h265parse ! nvh265dec",
        "qtdemux ! h264parse ! vaapih264dec ! vaapipostproc",
        "qtdemux ! h264parse ! vulkanh264dec",
        "decodebin",
    ];

    let mut descriptions = build_video_pipelines(
        location,
        &decoder_stages,
        fit_stage.as_str(),
        output_caps.as_str(),
    );

    // aspectratiocrop and videobox come from gst-plugins-good. Preserve a
    // correctness-first CPU fallback when that optional plugin is unavailable.
    if matches!(fit_mode, FitMode::Fill | FitMode::Cover | FitMode::Center) {
        let source_caps = format!(
            "video/x-raw,format=BGRx,pixel-aspect-ratio=1/1,framerate={}/1",
            fps
        );
        descriptions.extend(build_video_pipelines(
            location,
            &decoder_stages,
            "",
            source_caps.as_str(),
        ));
    }

    descriptions
}

fn build_video_pipelines(
    location: &str,
    decoder_stages: &[&str],
    fit_stage: &str,
    output_caps: &str,
) -> Vec<String> {
    decoder_stages
        .iter()
        .map(|decoder| {
            format!(
                "filesrc location=\"{}\" ! {} ! videoconvert{} ! videorate ! {} ! appsink name=sink sync=true max-buffers=1 drop=true",
                location, decoder, fit_stage, output_caps
            )
        })
        .collect()
}

fn filter_available_video_pipelines(descriptions: Vec<String>) -> Vec<String> {
    descriptions
        .into_iter()
        .filter(|pipeline| {
            if pipeline.contains("nvh264dec") && gst::ElementFactory::find("nvh264dec").is_none() {
                return false;
            }
            if pipeline.contains("nvh265dec") && gst::ElementFactory::find("nvh265dec").is_none() {
                return false;
            }
            if pipeline.contains("vaapih264dec") && gst::ElementFactory::find("vaapih264dec").is_none() {
                return false;
            }
            if pipeline.contains("vulkanh264dec") && gst::ElementFactory::find("vulkanh264dec").is_none() {
                return false;
            }
            true
        })
        .collect()
}

fn video_fit_stage(fit_mode: FitMode, width: u32, height: u32) -> String {
    match fit_mode {
        FitMode::Stretch => " ! videoscale n-threads=0 add-borders=false".into(),
        FitMode::Fill | FitMode::Cover => format!(
            " ! aspectratiocrop aspect-ratio={}/{} ! videoscale n-threads=0 add-borders=false",
            width, height
        ),
        FitMode::Fit | FitMode::Contain => {
            " ! videoscale n-threads=0 add-borders=true".into()
        }
        FitMode::Center => " ! videobox autocrop=true".into(),
        FitMode::ScaleDown => String::new(),
    }
}

fn video_output_caps(fit_mode: FitMode, width: u32, height: u32, fps: u32) -> String {
    let dimensions = if matches!(fit_mode, FitMode::ScaleDown) {
        String::new()
    } else {
        format!(",width={},height={}", width, height)
    };

    format!(
        "video/x-raw,format=BGRx,pixel-aspect-ratio=1/1{},framerate={}/1",
        dimensions, fps
    )
}

fn render_image_fit(image: &DynamicImage, out_w: u32, out_h: u32, fit_mode: FitMode) -> RgbaImage {
    render_rgba_fit(&image.to_rgba8(), out_w, out_h, fit_mode)
}

fn render_rgba_fit(image: &RgbaImage, out_w: u32, out_h: u32, fit_mode: FitMode) -> RgbaImage {
    let (resize_w, resize_h) =
        fitted_dimensions(image.width(), image.height(), out_w, out_h, fit_mode);

    let resized = if (resize_w, resize_h) == image.dimensions() {
        image.clone()
    } else {
        imageops::resize(image, resize_w, resize_h, FilterType::Lanczos3)
    };

    center_on_canvas(&resized, out_w, out_h)
}

fn fitted_dimensions(
    source_w: u32,
    source_h: u32,
    out_w: u32,
    out_h: u32,
    fit_mode: FitMode,
) -> (u32, u32) {
    debug_assert!(source_w > 0 && source_h > 0);
    debug_assert!(out_w > 0 && out_h > 0);

    match fit_mode {
        // Stretch is intentionally the only aspect-ratio-breaking mode.
        FitMode::Stretch => (out_w, out_h),

        // fill/cover enlarge just enough to cover the output, then crop evenly.
        FitMode::Fill | FitMode::Cover => {
            let (width, height) = scaled_dimensions(
                source_w,
                source_h,
                (out_w as f64 / source_w as f64).max(out_h as f64 / source_h as f64),
            );
            (width.max(out_w), height.max(out_h))
        }

        // fit/contain shrink or enlarge until the whole frame is visible.
        FitMode::Fit | FitMode::Contain => {
            let (width, height) = scaled_dimensions(
                source_w,
                source_h,
                (out_w as f64 / source_w as f64).min(out_h as f64 / source_h as f64),
            );
            (width.min(out_w), height.min(out_h))
        }

        // Center preserves source pixels and crops/pads equally on opposite sides.
        FitMode::Center => (source_w, source_h),

        // Scale-down is contain with upscaling disabled.
        FitMode::ScaleDown => {
            let (width, height) = scaled_dimensions(
                source_w,
                source_h,
                (out_w as f64 / source_w as f64)
                    .min(out_h as f64 / source_h as f64)
                    .min(1.0),
            );
            (
                width.min(source_w).min(out_w),
                height.min(source_h).min(out_h),
            )
        }
    }
}

fn scaled_dimensions(source_w: u32, source_h: u32, scale: f64) -> (u32, u32) {
    (
        (source_w as f64 * scale).round().max(1.0) as u32,
        (source_h as f64 * scale).round().max(1.0) as u32,
    )
}

fn center_on_canvas(image: &RgbaImage, out_w: u32, out_h: u32) -> RgbaImage {
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

    let mut last_visibility_refresh = Instant::now();
    let mut render_enabled = visibility.map(|v| v.should_render()).unwrap_or(true);
    let mut pending_render_state: Option<(bool, Instant)> = None;
    let mut primed_sample = Some(initial_sample);

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

            let observed = v.should_render();
            if observed == render_enabled {
                pending_render_state = None;
            } else {
                match pending_render_state {
                    Some((pending, since)) if pending == observed => {
                        if since.elapsed() >= Duration::from_millis(750) {
                            render_enabled = observed;
                            pending_render_state = None;
                        }
                    }
                    _ => pending_render_state = Some((observed, Instant::now())),
                }
            }
        }

        let should_render = render_enabled;

        let mut waited_for_release = false;

        let sample = primed_sample
            .take()
            .or_else(|| sink.try_pull_sample(gst::ClockTime::from_mseconds(frame_timeout_ms)));

        if let Some(sample) = sample {
            if should_render {
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
        } else if !should_render {
            std::thread::sleep(Duration::from_millis(30));
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
            let unique_id = BUFFER_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let buffer_path = std::env::temp_dir()
                .join(format!("papdieo-buffer-{}-{}-{}", process::id(), unique_id, index));

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

        let stride = usize::try_from(info.stride()[0])
            .map_err(|_| anyhow!("video frame has a negative stride"))?;
        let src = map.as_slice();

        if info.width() == width as u32
            && info.height() == height as u32
            && !matches!(fit_mode, FitMode::ScaleDown)
        {
            return self.write_bgrx_frame(slot_idx, src, stride, width, height);
        }

        let rgba = rgba_from_bgrx_frame(src, stride, info.width(), info.height())?;
        let rendered = render_rgba_fit(&rgba, width as u32, height as u32, fit_mode);
        self.write_rgba_image_frame(slot_idx, rendered.as_raw())
    }

    fn write_bgrx_frame(
        &mut self,
        slot_idx: usize,
        bgrx: &[u8],
        source_stride: usize,
        width: usize,
        height: usize,
    ) -> Result<()> {
        let row_bytes = width
            .checked_mul(4)
            .ok_or_else(|| anyhow!("video row size overflow"))?;
        let frame_bytes = row_bytes
            .checked_mul(height)
            .ok_or_else(|| anyhow!("video frame size overflow"))?;

        if frame_bytes > self.slots[slot_idx].frame_size {
            return Err(anyhow!("video frame is larger than renderer buffer"));
        }

        for row in 0..height {
            let src_start = row * source_stride;
            let src_end = src_start + row_bytes;
            let dst_start = row * row_bytes;
            if src_end > bgrx.len() {
                return Err(anyhow!("video frame stride exceeds buffer"));
            }
            self.slots[slot_idx].mmap[dst_start..dst_start + row_bytes]
                .copy_from_slice(&bgrx[src_start..src_end]);
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
    use super::{
        build_video_pipeline_descriptions, fitted_dimensions, render_rgba_fit, video_fit_stage,
        video_output_caps,
    };
    use crate::config::FitMode;
    use image::{Rgba, RgbaImage};

    #[test]
    fn video_modes_use_the_expected_streaming_fit_stage() {
        assert_eq!(
            video_fit_stage(FitMode::Stretch, 1920, 1080),
            " ! videoscale n-threads=0 add-borders=false"
        );
        assert_eq!(
            video_fit_stage(FitMode::Cover, 1920, 1080),
            " ! aspectratiocrop aspect-ratio=1920/1080 ! videoscale n-threads=0 add-borders=false"
        );
        assert_eq!(
            video_fit_stage(FitMode::Contain, 1920, 1080),
            " ! videoscale n-threads=0 add-borders=true"
        );
        assert_eq!(
            video_fit_stage(FitMode::Center, 1920, 1080),
            " ! videobox autocrop=true"
        );
        assert_eq!(video_fit_stage(FitMode::ScaleDown, 1920, 1080), "");
    }

    #[test]
    fn fitted_video_caps_force_square_pixels_and_output_dimensions() {
        let caps = video_output_caps(FitMode::Contain, 1920, 1080, 60);
        assert!(caps.contains("pixel-aspect-ratio=1/1"));
        assert!(caps.contains("width=1920,height=1080"));

        let scale_down_caps = video_output_caps(FitMode::ScaleDown, 1920, 1080, 60);
        assert!(scale_down_caps.contains("pixel-aspect-ratio=1/1"));
        assert!(!scale_down_caps.contains("width="));
        assert!(!scale_down_caps.contains("height="));
    }

    #[test]
    fn cover_video_has_optimized_pipelines_and_cpu_fallbacks() {
        let descriptions = build_video_pipeline_descriptions(
            "/tmp/demo.mp4",
            1920,
            1080,
            60,
            FitMode::Cover,
        );

        assert_eq!(descriptions.len(), 10);
        assert!(descriptions[..5]
            .iter()
            .all(|pipeline| pipeline.contains("aspectratiocrop aspect-ratio=1920/1080")));
        assert!(descriptions[5..]
            .iter()
            .all(|pipeline| !pipeline.contains("aspectratiocrop")));
        assert!(descriptions.iter().any(|pipeline| pipeline.contains("nvh264dec")));
        assert!(descriptions.iter().any(|pipeline| pipeline.contains("nvh265dec")));
    }

    #[test]
    fn every_fit_mode_has_the_expected_geometry() {
        let source = (400, 200);
        let output = (300, 300);

        assert_eq!(fit_dimensions(source, output, FitMode::Stretch), (300, 300));
        assert_eq!(fit_dimensions(source, output, FitMode::Fill), (600, 300));
        assert_eq!(fit_dimensions(source, output, FitMode::Cover), (600, 300));
        assert_eq!(fit_dimensions(source, output, FitMode::Fit), (300, 150));
        assert_eq!(fit_dimensions(source, output, FitMode::Contain), (300, 150));
        assert_eq!(fit_dimensions(source, output, FitMode::Center), (400, 200));
        assert_eq!(
            fit_dimensions(source, output, FitMode::ScaleDown),
            (300, 150)
        );
    }

    #[test]
    fn aliases_render_identically() {
        let image = test_image(4, 2);

        assert_eq!(
            render_rgba_fit(&image, 3, 3, FitMode::Fill),
            render_rgba_fit(&image, 3, 3, FitMode::Cover)
        );
        assert_eq!(
            render_rgba_fit(&image, 3, 3, FitMode::Fit),
            render_rgba_fit(&image, 3, 3, FitMode::Contain)
        );
    }

    #[test]
    fn contain_letterboxes_while_cover_and_stretch_fill_the_output() {
        let image = RgbaImage::from_pixel(4, 2, Rgba([255, 255, 255, 255]));
        let contain = render_rgba_fit(&image, 4, 4, FitMode::Contain);
        let cover = render_rgba_fit(&image, 4, 4, FitMode::Cover);
        let stretch = render_rgba_fit(&image, 4, 4, FitMode::Stretch);

        assert_eq!(contain.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(contain.get_pixel(0, 1).0, [255, 255, 255, 255]);
        assert_eq!(cover.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(stretch.get_pixel(0, 0).0, [255, 255, 255, 255]);
    }

    #[test]
    fn scale_down_does_not_upscale_smaller_images() {
        let image = RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 255]));

        let rendered = render_rgba_fit(&image, 3, 3, FitMode::ScaleDown);

        assert_eq!(rendered.get_pixel(1, 1).0, [255, 255, 255, 255]);
        assert_eq!(rendered.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(rendered.get_pixel(2, 2).0, [0, 0, 0, 0]);
    }

    #[test]
    fn center_crops_without_scaling() {
        let image = test_image(3, 1);
        let rendered = render_rgba_fit(&image, 1, 1, FitMode::Center);

        assert_eq!(rendered.get_pixel(0, 0), image.get_pixel(1, 0));
    }

    fn fit_dimensions(source: (u32, u32), output: (u32, u32), mode: FitMode) -> (u32, u32) {
        fitted_dimensions(source.0, source.1, output.0, output.1, mode)
    }

    fn test_image(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x * 40) as u8, (y * 80) as u8, 127, 255])
        })
    }
}
