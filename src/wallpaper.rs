use anyhow::{anyhow, Context, Result};
use crate::config::FitMode;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use image::{imageops::FilterType, RgbaImage};
use memmap2::MmapMut;
use std::{
    fs::File,
    fs::OpenOptions,
    os::fd::AsFd,
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
        });
    }

    event_queue
        .roundtrip(&mut state)
        .context("failed to discover monitor names")?;

    let selected_output = state.select_output()?;

    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        Some(&selected_output),
        zwlr_layer_shell_v1::Layer::Background,
        "hyprwall".into(),
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
        event_queue
            .blocking_dispatch(&mut state)
            .context("failed during initial Wayland dispatch")?;
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
        )?;
    } else {
        draw_image(&state, &surface, &mut frame_renderer, fit_mode)?;
        while !state.exit {
            event_queue
                .blocking_dispatch(&mut state)
                .context("failed during Wayland event dispatch")?;
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

    let descriptions = [
        format!(
            "filesrc location=\"{}\" ! qtdemux ! h264parse ! nvh264dec ! videoconvert ! videoscale{} ! videorate ! video/x-raw,format=BGRx,width={},height={},framerate={}/1 ! appsink name=sink sync=true max-buffers=1 drop=true",
            location, videoscale_options(fit_mode), width, height, fps
        ),
        format!(
            "filesrc location=\"{}\" ! qtdemux ! h264parse ! vulkanh264dec ! videoconvert ! videoscale{} ! videorate ! video/x-raw,format=BGRx,width={},height={},framerate={}/1 ! appsink name=sink sync=true max-buffers=1 drop=true",
            location, videoscale_options(fit_mode), width, height, fps
        ),
        format!(
            "filesrc location=\"{}\" ! decodebin ! videoconvert ! videoscale{} ! videorate ! video/x-raw,format=BGRx,width={},height={},framerate={}/1 ! appsink name=sink sync=true max-buffers=1 drop=true",
            location, videoscale_options(fit_mode), width, height, fps
        ),
    ];

    let mut last_error: Option<anyhow::Error> = None;
    for pipeline_desc in descriptions {
        match run_video_pipeline(
            &pipeline_desc,
            width,
            height,
            surface,
            renderer,
            event_queue,
            state,
            visibility.as_ref(),
            frame_timeout_ms,
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

fn videoscale_options(fit_mode: FitMode) -> &'static str {
    match fit_mode {
        FitMode::Fit | FitMode::Contain => " add-borders=true",
        _ => "",
    }
}

fn render_image_fit(
    image: &image::DynamicImage,
    out_w: u32,
    out_h: u32,
    fit_mode: FitMode,
) -> RgbaImage {
    match fit_mode {
        FitMode::Stretch => image.resize_exact(out_w, out_h, FilterType::Lanczos3).to_rgba8(),
        FitMode::Fit | FitMode::Contain => {
            let resized = image.resize(out_w, out_h, FilterType::Lanczos3).to_rgba8();
            let mut canvas = RgbaImage::new(out_w, out_h);
            let x = ((out_w as i64 - resized.width() as i64) / 2).max(0) as u32;
            let y = ((out_h as i64 - resized.height() as i64) / 2).max(0) as u32;
            image::imageops::overlay(&mut canvas, &resized, x as i64, y as i64);
            canvas
        }
        FitMode::Fill | FitMode::Cover => {
            let scale = f64::max(
                out_w as f64 / image.width() as f64,
                out_h as f64 / image.height() as f64,
            );
            let rw = (image.width() as f64 * scale).round().max(out_w as f64) as u32;
            let rh = (image.height() as f64 * scale).round().max(out_h as f64) as u32;
            let resized = image.resize_exact(rw, rh, FilterType::Lanczos3).to_rgba8();
            let x = (rw.saturating_sub(out_w)) / 2;
            let y = (rh.saturating_sub(out_h)) / 2;
            image::imageops::crop_imm(&resized, x, y, out_w, out_h).to_image()
        }
    }
}

fn run_video_pipeline(
    pipeline_desc: &str,
    width: u32,
    height: u32,
    surface: &wl_surface::WlSurface,
    renderer: &mut FrameRenderer,
    event_queue: &mut EventQueue<AppState>,
    state: &mut AppState,
    visibility: Option<&HyprlandVisibility>,
    frame_timeout_ms: u64,
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

    let initial_bytes = bgrx_from_sample(&initial_sample, width as usize, height as usize)?;
    draw_video_frame(&initial_bytes, surface, renderer)?;

    let mut is_paused = false;
    let mut last_visibility_refresh = Instant::now();

    while !state.exit {
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

        if should_render {
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(frame_timeout_ms)) {
                let bytes = bgrx_from_sample(&sample, width as usize, height as usize)?;
                draw_video_frame(&bytes, surface, renderer)?;
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

        event_queue
            .dispatch_pending(state)
            .context("failed dispatching Wayland events")?;
        event_queue.flush().ok();
    }

    pipeline.set_state(gst::State::Null).ok();
    Ok(())
}

fn bgrx_from_sample(sample: &gst::Sample, width: usize, height: usize) -> Result<Vec<u8>> {
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
    let mut out = vec![0_u8; width * height * 4];

    for row in 0..height {
        let src_start = row * stride;
        let src_end = src_start + width * 4;
        let dst_start = row * width * 4;
        let dst_end = dst_start + width * 4;
        out[dst_start..dst_end].copy_from_slice(&src[src_start..src_end]);
    }

    Ok(out)
}

fn draw_video_frame(
    bgrx_bytes: &[u8],
    surface: &wl_surface::WlSurface,
    renderer: &mut FrameRenderer,
) -> Result<()> {
    renderer.write_bgrx_frame(bgrx_bytes)?;

    surface.attach(Some(&renderer.buffer), 0, 0);
    surface.damage_buffer(0, 0, renderer.width as i32, renderer.height as i32);
    surface.commit();

    Ok(())
}

fn draw_image_frame(
    rgba_bytes: &[u8],
    surface: &wl_surface::WlSurface,
    renderer: &mut FrameRenderer,
) -> Result<()> {
    renderer.write_rgba_image_frame(rgba_bytes)?;

    surface.attach(Some(&renderer.buffer), 0, 0);
    surface.damage_buffer(0, 0, renderer.width as i32, renderer.height as i32);
    surface.commit();

    Ok(())
}

struct FrameRenderer {
    width: u32,
    height: u32,
    frame_size: usize,
    mmap: MmapMut,
    _file: File,
    _pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
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

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open("/tmp/hyprwall-buffer")
            .context("failed to create shared memory buffer file")?;
        file.set_len(frame_size as u64)?;

        let mmap = unsafe { MmapMut::map_mut(&file) }.context("failed to map shared memory")?;

        let pool = shm.create_pool(file.as_fd(), frame_size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        );

        Ok(Self {
            width,
            height,
            frame_size,
            mmap,
            _file: file,
            _pool: pool,
            buffer,
        })
    }

    fn write_bgrx_frame(&mut self, bgrx: &[u8]) -> Result<()> {
        if bgrx.len() > self.frame_size {
            return Err(anyhow!("frame is larger than renderer buffer"));
        }

        self.mmap[..bgrx.len()].copy_from_slice(bgrx);
        Ok(())
    }

    fn write_rgba_image_frame(&mut self, rgba: &[u8]) -> Result<()> {
        if rgba.len() > self.frame_size {
            return Err(anyhow!("image frame is larger than renderer buffer"));
        }

        for (dst, px) in self.mmap[..rgba.len()].chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            dst[0] = px[2];
            dst[1] = px[1];
            dst[2] = px[0];
            dst[3] = 255;
        }
        Ok(())
    }
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
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let clients = value.as_array()?;

    let has_relevant_window = clients.iter().any(|client| {
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

        match target_monitor_id {
            Some(target_id) => client.get("monitor").and_then(|v| v.as_i64()) == Some(target_id),
            None => true,
        }
    });

    Some(!has_relevant_window)
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
            requested_monitor,
            outputs: Vec::new(),
            width: 1920,
            height: 1080,
            configured: false,
            exit: false,
        }
    }

    fn select_output(&self) -> Result<wl_output::WlOutput> {
        if let Some(requested) = &self.requested_monitor {
            if let Some(found) = self
                .outputs
                .iter()
                .find(|out| out.name.as_deref() == Some(requested.as_str()))
            {
                return Ok(found.output.clone());
            }

            let available: Vec<String> = self
                .outputs
                .iter()
                .filter_map(|out| out.name.clone())
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
        if let wl_output::Event::Name { name } = event {
            if let Some(output) = state.outputs.iter_mut().find(|o| o.global_name == *data) {
                output.name = Some(name);
            }
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

impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
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
