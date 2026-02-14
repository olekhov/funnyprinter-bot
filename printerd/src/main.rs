use std::{
    collections::HashMap,
    io::Cursor,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use clap::Parser;
use funnyprint_proto::{MAX_DOTS_PER_LINE, PackedLine, discover_candidates, dpi, print_job};
use funnyprint_render::{TextRenderOptions, image_to_packed_lines, px_to_mm, render_text_to_image};
use image::{DynamicImage, GrayImage, ImageFormat, Luma, imageops::FilterType};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(name = "printerd")]
#[command(about = "HTTP print daemon for FunnyPrint BLE printers")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: String,
    #[arg(long)]
    default_address: Option<String>,
    #[arg(long)]
    api_token: Option<String>,
}

#[derive(Clone)]
struct AppState {
    api_token: Option<String>,
    default_address: Option<String>,
    renders: Arc<RwLock<HashMap<String, RenderArtifact>>>,
    jobs: Arc<RwLock<HashMap<String, JobRecord>>>,
    render_seq: Arc<AtomicU64>,
    job_seq: Arc<AtomicU64>,
    queue_tx: mpsc::Sender<PrintCommand>,
}

#[derive(Clone)]
struct RenderArtifact {
    preview_png: Vec<u8>,
    packed_lines: Vec<PackedLine>,
    density: u8,
    address_override: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum JobStatus {
    Queued,
    Printing,
    Done,
    Failed,
}

#[derive(Clone, Serialize)]
struct JobRecord {
    id: String,
    render_id: String,
    address: String,
    density: u8,
    status: JobStatus,
    error: Option<String>,
}

#[derive(Debug)]
struct PrintCommand {
    job_id: String,
    render_id: String,
    address: String,
    density: u8,
}

#[derive(Debug, Deserialize)]
struct ScanQuery {
    seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RenderTextRequest {
    text: String,
    font_path: String,
    width_px: Option<u32>,
    height_px: Option<u32>,
    x_px: Option<i32>,
    y_px: Option<i32>,
    font_size_px: Option<f32>,
    line_spacing: Option<f32>,
    threshold: Option<u8>,
    invert: Option<bool>,
    trim_blank_top_bottom: Option<bool>,
    density: Option<u8>,
    address: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum DitherMethod {
    Threshold,
    FloydSteinberg,
}

#[derive(Debug, Deserialize)]
struct RenderImageRequest {
    image_base64: String,
    width_px: Option<u32>,
    max_height_px: Option<u32>,
    threshold: Option<u8>,
    dither_method: Option<DitherMethod>,
    invert: Option<bool>,
    trim_blank_top_bottom: Option<bool>,
    density: Option<u8>,
    address: Option<String>,
}

#[derive(Debug, Serialize)]
struct RenderTextResponse {
    render_id: String,
    width_px: u32,
    height_px: u32,
    width_mm: f32,
    height_mm: f32,
    packed_lines: usize,
    preview_url: String,
}

#[derive(Debug, Deserialize)]
struct PrintRequest {
    render_id: String,
    address: Option<String>,
    density: Option<u8>,
}

#[derive(Debug, Serialize)]
struct PrintResponse {
    job_id: String,
    status_url: String,
}

#[derive(Debug, Deserialize)]
struct WaitQuery {
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug, Serialize)]
struct ScanDevice {
    address: String,
    local_name: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let args = Args::parse();
    let listen_addr: SocketAddr = args.listen.parse()?;

    let (tx, rx) = mpsc::channel::<PrintCommand>(64);

    let state = AppState {
        api_token: args.api_token,
        default_address: args.default_address,
        renders: Arc::new(RwLock::new(HashMap::new())),
        jobs: Arc::new(RwLock::new(HashMap::new())),
        render_seq: Arc::new(AtomicU64::new(1)),
        job_seq: Arc::new(AtomicU64::new(1)),
        queue_tx: tx,
    };

    tokio::spawn(worker_loop(state.clone(), rx));

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/printers/scan", get(scan_printers))
        .route("/api/v1/renders/text", post(render_text))
        .route("/api/v1/renders/image", post(render_image))
        .route("/api/v1/renders/{id}/preview", get(get_preview))
        .route("/api/v1/print", post(queue_print))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route("/api/v1/jobs/{id}/wait", get(wait_job))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    info!("printerd listening on http://{}", listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn scan_printers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScanQuery>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }

    let secs = query.seconds.unwrap_or(3).clamp(1, 15);
    info!(scan_seconds = secs, "starting BLE scan");
    match discover_candidates(Duration::from_secs(secs)).await {
        Ok(list) => {
            let devices: Vec<ScanDevice> = list
                .into_iter()
                .map(|d| ScanDevice {
                    address: d.address,
                    local_name: d.local_name,
                })
                .collect();
            info!(found = devices.len(), "BLE scan completed");
            (StatusCode::OK, axum::Json(devices)).into_response()
        }
        Err(err) => {
            error!(error = %err, "BLE scan failed");
            error_response(StatusCode::BAD_GATEWAY, format!("scan failed: {err}"))
        }
    }
}

async fn render_text(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<RenderTextRequest>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }

    if req.text.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "text is empty".to_string());
    }

    let width_px = req.width_px.unwrap_or(MAX_DOTS_PER_LINE as u32);
    if width_px as usize > MAX_DOTS_PER_LINE {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("width_px exceeds max {}", MAX_DOTS_PER_LINE),
        );
    }

    let opts = TextRenderOptions {
        width_px,
        height_px: req.height_px.unwrap_or(192),
        x_px: req.x_px.unwrap_or(0),
        y_px: req.y_px.unwrap_or(0),
        font_size_px: req.font_size_px.unwrap_or(48.0),
        line_spacing: req.line_spacing.unwrap_or(1.0),
        threshold: req.threshold.unwrap_or(180),
        invert: req.invert.unwrap_or(false),
        trim_blank_top_bottom: req.trim_blank_top_bottom.unwrap_or(true),
    };

    let font_path = PathBuf::from(req.font_path);
    let image = match render_text_to_image(&req.text, &font_path, &opts) {
        Ok(v) => v,
        Err(err) => {
            return error_response(StatusCode::BAD_REQUEST, format!("render failed: {err}"));
        }
    };

    let packed = image_to_packed_lines(&image, opts.threshold, opts.trim_blank_top_bottom);
    if packed.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "render result is blank after trim".to_string(),
        );
    }

    let png = match encode_png(&image) {
        Ok(v) => v,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("png encode failed: {err}"),
            );
        }
    };

    let density = req.density.unwrap_or(3);
    if density > 7 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "density must be in 0..=7".to_string(),
        );
    }

    let render_id = next_id("r", &state.render_seq);
    let artifact = RenderArtifact {
        preview_png: png,
        packed_lines: packed.clone(),
        density,
        address_override: req.address,
    };

    state
        .renders
        .write()
        .await
        .insert(render_id.clone(), artifact);
    info!(
        render_id = %render_id,
        width_px = image.width(),
        height_px = image.height(),
        packed_lines = packed.len(),
        "rendered text preview"
    );

    let resp = RenderTextResponse {
        render_id: render_id.clone(),
        width_px: image.width(),
        height_px: image.height(),
        width_mm: px_to_mm(image.width(), dpi()),
        height_mm: px_to_mm(image.height(), dpi()),
        packed_lines: packed.len(),
        preview_url: format!("/api/v1/renders/{render_id}/preview"),
    };

    (StatusCode::OK, axum::Json(resp)).into_response()
}

async fn render_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<RenderImageRequest>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }

    let width_px = req.width_px.unwrap_or(MAX_DOTS_PER_LINE as u32);
    if width_px == 0 || width_px as usize > MAX_DOTS_PER_LINE {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("width_px must be in 1..={}", MAX_DOTS_PER_LINE),
        );
    }

    let image_bytes = match base64::engine::general_purpose::STANDARD.decode(req.image_base64) {
        Ok(v) => v,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid image_base64: {err}"),
            );
        }
    };

    let dyn_img = match image::load_from_memory(&image_bytes) {
        Ok(v) => v,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid image data: {err}"),
            );
        }
    };

    let gray = dyn_img.to_luma8();
    let src_w = gray.width().max(1);
    let src_h = gray.height().max(1);
    let mut target_h = ((src_h as f32 * width_px as f32) / src_w as f32).round() as u32;
    target_h = target_h.max(1);
    if let Some(max_h) = req.max_height_px {
        target_h = target_h.min(max_h.max(1));
    }

    let resized = image::imageops::resize(&gray, width_px, target_h, FilterType::Lanczos3);
    let threshold = req.threshold.unwrap_or(180);
    let dither = req.dither_method.unwrap_or(DitherMethod::FloydSteinberg);
    let invert = req.invert.unwrap_or(false);
    let trim_blank = req.trim_blank_top_bottom.unwrap_or(true);

    let bw_preview = binarize_preview(&resized, threshold, dither, invert);
    let packed_lines = pack_bw_image(&bw_preview, trim_blank);
    if packed_lines.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "render result is blank after trim".to_string(),
        );
    }

    let preview_png = match encode_png(&bw_preview) {
        Ok(v) => v,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("png encode failed: {err}"),
            );
        }
    };

    let density = req.density.unwrap_or(3);
    if density > 7 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "density must be in 0..=7".to_string(),
        );
    }

    let render_id = next_id("r", &state.render_seq);
    let artifact = RenderArtifact {
        preview_png,
        packed_lines: packed_lines.clone(),
        density,
        address_override: req.address,
    };
    state
        .renders
        .write()
        .await
        .insert(render_id.clone(), artifact);

    info!(
        render_id = %render_id,
        width_px = bw_preview.width(),
        height_px = bw_preview.height(),
        packed_lines = packed_lines.len(),
        "rendered image preview"
    );

    let resp = RenderTextResponse {
        render_id: render_id.clone(),
        width_px: bw_preview.width(),
        height_px: bw_preview.height(),
        width_mm: px_to_mm(bw_preview.width(), dpi()),
        height_mm: px_to_mm(bw_preview.height(), dpi()),
        packed_lines: packed_lines.len(),
        preview_url: format!("/api/v1/renders/{render_id}/preview"),
    };

    (StatusCode::OK, axum::Json(resp)).into_response()
}

async fn get_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }

    let renders = state.renders.read().await;
    let Some(artifact) = renders.get(&id) else {
        return error_response(StatusCode::NOT_FOUND, "render not found".to_string());
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/png")],
        artifact.preview_png.clone(),
    )
        .into_response()
}

async fn queue_print(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<PrintRequest>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }

    let Some(artifact) = state.renders.read().await.get(&req.render_id).cloned() else {
        return error_response(StatusCode::NOT_FOUND, "render not found".to_string());
    };

    let address = match req
        .address
        .or(artifact.address_override)
        .or_else(|| state.default_address.clone())
    {
        Some(v) => v,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "address is missing and no --default-address configured".to_string(),
            );
        }
    };

    let density = req.density.unwrap_or(artifact.density);
    if density > 7 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "density must be in 0..=7".to_string(),
        );
    }

    let job_id = next_id("j", &state.job_seq);
    let record = JobRecord {
        id: job_id.clone(),
        render_id: req.render_id.clone(),
        address: address.clone(),
        density,
        status: JobStatus::Queued,
        error: None,
    };
    state.jobs.write().await.insert(job_id.clone(), record);
    info!(
        job_id = %job_id,
        render_id = %req.render_id,
        address = %address,
        density = density,
        "queued print job"
    );

    let cmd = PrintCommand {
        job_id: job_id.clone(),
        render_id: req.render_id,
        address,
        density,
    };

    if state.queue_tx.send(cmd).await.is_err() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "print queue is not available".to_string(),
        );
    }

    let resp = PrintResponse {
        job_id: job_id.clone(),
        status_url: format!("/api/v1/jobs/{job_id}"),
    };

    (StatusCode::ACCEPTED, axum::Json(resp)).into_response()
}

async fn wait_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }

    let timeout_secs = query.timeout_seconds.unwrap_or(20).clamp(1, 120);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

    loop {
        let maybe_job = { state.jobs.read().await.get(&id).cloned() };
        let Some(job) = maybe_job else {
            return error_response(StatusCode::NOT_FOUND, "job not found".to_string());
        };

        match job.status {
            JobStatus::Done | JobStatus::Failed => {
                return (StatusCode::OK, axum::Json(job)).into_response();
            }
            JobStatus::Queued | JobStatus::Printing => {}
        }

        if tokio::time::Instant::now() >= deadline {
            return (StatusCode::ACCEPTED, axum::Json(job)).into_response();
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn get_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }

    let jobs = state.jobs.read().await;
    let Some(job) = jobs.get(&id) else {
        return error_response(StatusCode::NOT_FOUND, "job not found".to_string());
    };

    (StatusCode::OK, axum::Json(job)).into_response()
}

async fn worker_loop(state: AppState, mut rx: mpsc::Receiver<PrintCommand>) {
    while let Some(cmd) = rx.recv().await {
        info!(
            job_id = %cmd.job_id,
            render_id = %cmd.render_id,
            address = %cmd.address,
            density = cmd.density,
            "starting print job"
        );
        {
            let mut jobs = state.jobs.write().await;
            if let Some(job) = jobs.get_mut(&cmd.job_id) {
                job.status = JobStatus::Printing;
                job.error = None;
            }
        }

        let packed = {
            let renders = state.renders.read().await;
            renders.get(&cmd.render_id).map(|r| r.packed_lines.clone())
        };

        let result = match packed {
            Some(lines) => print_job(&cmd.address, &lines, cmd.density).await,
            None => Err(anyhow::anyhow!("render {} not found", cmd.render_id)),
        };

        let mut jobs = state.jobs.write().await;
        if let Some(job) = jobs.get_mut(&cmd.job_id) {
            match result {
                Ok(()) => {
                    job.status = JobStatus::Done;
                    job.error = None;
                    info!(job_id = %cmd.job_id, "print job completed");
                }
                Err(err) => {
                    job.status = JobStatus::Failed;
                    job.error = Some(err.to_string());
                    warn!(job_id = %cmd.job_id, error = %err, "print job failed");
                }
            }
        }
    }
}

fn encode_png(image: &GrayImage) -> anyhow::Result<Vec<u8>> {
    let dyn_img = DynamicImage::ImageLuma8(image.clone());
    let mut cursor = Cursor::new(Vec::<u8>::new());
    dyn_img.write_to(&mut cursor, ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

fn binarize_preview(
    gray: &GrayImage,
    threshold: u8,
    method: DitherMethod,
    invert: bool,
) -> GrayImage {
    match method {
        DitherMethod::Threshold => threshold_binarize(gray, threshold, invert),
        DitherMethod::FloydSteinberg => floyd_steinberg_binarize(gray, threshold, invert),
    }
}

fn threshold_binarize(gray: &GrayImage, threshold: u8, invert: bool) -> GrayImage {
    let mut out = GrayImage::new(gray.width(), gray.height());
    for (x, y, p) in gray.enumerate_pixels() {
        let mut v = p.0[0];
        if invert {
            v = 255 - v;
        }
        let bw = if v <= threshold { 0u8 } else { 255u8 };
        out.put_pixel(x, y, Luma([bw]));
    }
    out
}

fn floyd_steinberg_binarize(gray: &GrayImage, threshold: u8, invert: bool) -> GrayImage {
    let w = gray.width() as usize;
    let h = gray.height() as usize;
    let mut buf = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut v = gray.get_pixel(x as u32, y as u32).0[0] as f32;
            if invert {
                v = 255.0 - v;
            }
            buf[y * w + x] = v;
        }
    }

    let mut out = GrayImage::new(gray.width(), gray.height());
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            let old = buf[idx].clamp(0.0, 255.0);
            let new = if old <= threshold as f32 { 0.0 } else { 255.0 };
            let err = old - new;
            out.put_pixel(x as u32, y as u32, Luma([new as u8]));

            if x + 1 < w {
                buf[idx + 1] += err * 7.0 / 16.0;
            }
            if y + 1 < h {
                if x > 0 {
                    buf[idx + w - 1] += err * 3.0 / 16.0;
                }
                buf[idx + w] += err * 5.0 / 16.0;
                if x + 1 < w {
                    buf[idx + w + 1] += err * 1.0 / 16.0;
                }
            }
        }
    }
    out
}

fn pack_bw_image(img: &GrayImage, trim_blank: bool) -> Vec<PackedLine> {
    let width = img.width().min(MAX_DOTS_PER_LINE as u32) as usize;
    let height = img.height() as usize;
    let bytes_per_line = MAX_DOTS_PER_LINE / 8;
    let mut out = Vec::with_capacity(height.div_ceil(2));

    for y in (0..height).step_by(2) {
        let mut line = [0u8; 96];
        for row in 0..2 {
            let yy = y + row;
            if yy >= height {
                continue;
            }
            for x in 0..width {
                let px = img.get_pixel(x as u32, yy as u32).0[0];
                if px == 0 {
                    let byte_idx = row * bytes_per_line + (x / 8);
                    let bit = 7 - (x % 8);
                    line[byte_idx] |= 1u8 << bit;
                }
            }
        }
        out.push(line);
    }

    if !trim_blank {
        return out;
    }
    let first = out.iter().position(|l| l.iter().any(|b| *b != 0));
    let last = out.iter().rposition(|l| l.iter().any(|b| *b != 0));
    match (first, last) {
        (Some(start), Some(end)) => out[start..=end].to_vec(),
        _ => Vec::new(),
    }
}

fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(expected) = &state.api_token else {
        return Ok(());
    };

    let got = headers
        .get("x-api-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    if got == expected {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized".to_string(),
        ))
    }
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, axum::Json(ErrorBody { error: message })).into_response()
}

fn next_id(prefix: &str, seq: &AtomicU64) -> String {
    let n = seq.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{n}")
}
