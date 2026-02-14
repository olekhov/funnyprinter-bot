use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(name = "ai-service")]
#[command(about = "AI image generation service for sticker bot")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8090")]
    listen: String,
    #[arg(long)]
    openai_api_key: Option<String>,
    #[arg(long, default_value = "gpt-image-1-mini")]
    model: String,
    #[arg(long)]
    api_token: Option<String>,
}

#[derive(Clone)]
struct AppState {
    http: Client,
    openai_api_key: String,
    model: String,
    api_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GenerateRequest {
    prompt: String,
    size: Option<String>,
    quality: Option<String>,
    n: Option<u8>,
}

#[derive(Debug, Serialize)]
struct GenerateResponse {
    image_base64: String,
    revised_prompt: Option<String>,
    model: String,
    size: String,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug, Serialize)]
struct OpenAiImageRequest {
    model: String,
    prompt: String,
    size: String,
    quality: String,
    n: u8,
}

#[derive(Debug, Deserialize)]
struct OpenAiImageResponse {
    data: Vec<OpenAiImageData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiImageData {
    b64_json: Option<String>,
    revised_prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiErrorBody,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorBody {
    message: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let args = Args::parse();
    let openai_api_key = match args
        .openai_api_key
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
    {
        Some(v) => v,
        None => bail!("openai api key is missing: pass --openai-api-key or set OPENAI_API_KEY"),
    };
    let addr: SocketAddr = args.listen.parse().context("invalid --listen address")?;

    let state = Arc::new(AppState {
        http: Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .context("failed to build http client")?,
        openai_api_key,
        model: args.model,
        api_token: args.api_token,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/generate", post(generate))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(listen = %addr, "ai-service started");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn generate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<GenerateRequest>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }

    if req.prompt.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "prompt is empty");
    }

    let size = req.size.unwrap_or_else(|| "1024x1024".to_string());
    if !is_allowed_size(&size) {
        return error_response(StatusCode::BAD_REQUEST, "unsupported size");
    }

    let quality = req.quality.unwrap_or_else(|| "low".to_string());
    if !matches!(quality.as_str(), "low" | "medium" | "high") {
        return error_response(StatusCode::BAD_REQUEST, "quality must be low|medium|high");
    }

    let n = req.n.unwrap_or(1).clamp(1, 1);

    /*
    let style_prefix = "Minimal black-and-white line art for thermal sticker printer. Thin clean outlines, white background, no fills, no shading, no grayscale, high contrast.";
    let final_prompt = format!("{} User request: {}", style_prefix, req.prompt.trim());
    */

    let style_prefix = "Чёрно-белое изображение. 
Только чёрные линии (#000000). 
Фон — чистый сплошной белый цвет (#FFFFFF), ровная плоская заливка.
Без градиентов, без теней, без виньетки, без текстуры, без освещения, без серых оттенков.
Высокий контраст, жёсткие края.

Black and white vector illustration.
Background: pure solid white (#FFFFFF), flat fill.
No gradients, no shadows, no vignette, no texture, no lighting, no gray background.
Hard edges, high contrast.";


    //let style_prefix = "Чёрно-белое изображение, чёткие чёрные линии, фон только белый. Без закрашивания, без теней, высокий контраст";
    let final_prompt = format!("Стиль изображения: {}. Содержимое изображения: {}", style_prefix, req.prompt.trim()); 
    let oa_req = OpenAiImageRequest {
        model: state.model.clone(),
        prompt: final_prompt,
        size: size.clone(),
        quality,
        n,
    };

    match generate_openai_image(&state, oa_req).await {
        Ok((image_base64, revised_prompt)) => {
            info!(model = %state.model, size = %size, "image generated");
            let out = GenerateResponse {
                image_base64,
                revised_prompt,
                model: state.model.clone(),
                size,
            };
            (StatusCode::OK, axum::Json(out)).into_response()
        }
        Err(err) => {
            error!(error = %err, "image generation failed");
            error_response(
                StatusCode::BAD_GATEWAY,
                &format!("generation failed: {err}"),
            )
        }
    }
}

async fn generate_openai_image(
    state: &AppState,
    req: OpenAiImageRequest,
) -> Result<(String, Option<String>)> {
    let resp = state
        .http
        .post("https://api.openai.com/v1/images/generations")
        .bearer_auth(&state.openai_api_key)
        .json(&req)
        .send()
        .await
        .context("failed to call OpenAI API")?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .context("failed to read OpenAI response")?;

    if !status.is_success() {
        if let Ok(err_env) = serde_json::from_slice::<OpenAiErrorEnvelope>(&bytes) {
            bail!("openai error {}: {}", status, err_env.error.message);
        }
        let body = String::from_utf8_lossy(&bytes);
        bail!("openai error {}: {}", status, body);
    }

    let decoded: OpenAiImageResponse =
        serde_json::from_slice(&bytes).context("failed to decode OpenAI image response")?;
    let first = decoded
        .data
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("OpenAI response has no image data"))?;
    let b64 = first
        .b64_json
        .ok_or_else(|| anyhow::anyhow!("OpenAI response has no b64_json"))?;

    Ok((b64, first.revised_prompt))
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
        Err(error_response(StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

fn is_allowed_size(size: &str) -> bool {
    matches!(size, "1024x1024" | "1024x1536" | "1536x1024")
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(ErrorBody {
            error: message.to_string(),
        }),
    )
        .into_response()
}
