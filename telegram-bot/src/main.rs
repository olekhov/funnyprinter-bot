use std::{path::PathBuf, sync::Arc, time::Duration};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use clap::Parser;
use serde::{Deserialize, Serialize};
use teloxide::{
    dispatching::UpdateFilterExt,
    prelude::*,
    types::{
        ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, KeyboardButton,
        KeyboardMarkup,
    },
    utils::command::BotCommands,
};
use tokio::sync::RwLock;
use tokio_rusqlite::{Connection, rusqlite};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(name = "telegram-bot")]
struct Args {
    #[arg(long, default_value = "bot-config.toml")]
    config: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct Config {
    telegram_token: String,
    sqlite_path: String,
    printerd: PrinterdConfig,
    ai_service: AiServiceConfig,
    sticker: StickerConfig,
    image_sticker: ImageStickerConfig,
    access: AccessConfig,
}

#[derive(Debug, Clone, Deserialize)]
struct PrinterdConfig {
    base_url: String,
    api_token: Option<String>,
    address: Option<String>,
    wait_job_timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct StickerConfig {
    font_path: String,
    printer_width_px: u32,
    margin_left_px: u32,
    margin_right_px: u32,
    margin_top_px: u32,
    margin_bottom_px: u32,
    min_font_size_px: f32,
    max_font_size_px: f32,
    line_spacing: f32,
    threshold: u8,
    density: u8,
    invert: bool,
    trim_blank_top_bottom: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ImageStickerConfig {
    threshold: u8,
    dither_method: DitherMethod,
    density: u8,
    invert: bool,
    trim_blank_top_bottom: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DitherMethod {
    Threshold,
    FloydSteinberg,
}

#[derive(Debug, Clone, Deserialize)]
struct AccessConfig {
    allowed_user_ids: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct AiServiceConfig {
    base_url: String,
    api_token: Option<String>,
    default_size: Option<String>,
    default_quality: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    SimpleText,
    AiImage,
}

#[derive(Clone)]
struct AppState {
    cfg: Config,
    db: Db,
    printerd: PrinterdClient,
    ai: AiServiceClient,
    font: FontArc,
    user_modes: Arc<RwLock<std::collections::HashMap<i64, InputMode>>>,
}

#[derive(Clone)]
struct Db {
    conn: Arc<Connection>,
}

#[derive(Clone)]
struct PrinterdClient {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
    default_address: Option<String>,
}

#[derive(Clone)]
struct AiServiceClient {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
    default_size: String,
    default_quality: String,
}

#[derive(Debug, Clone)]
struct StickerRecord {
    id: i64,
    kind: StickerKind,
    text: String,
    width_px: u32,
    height_px: u32,
    x_px: i32,
    y_px: i32,
    font_size_px: f32,
    threshold: u8,
    invert: bool,
    trim_blank_top_bottom: bool,
    density: u8,
    dither_method: Option<DitherMethod>,
    source_image_bytes: Option<Vec<u8>>,
    preview_png: Vec<u8>,
    created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StickerKind {
    Text,
    Image,
}

#[derive(Debug, Serialize)]
struct RenderTextRequest {
    text: String,
    font_path: String,
    width_px: u32,
    height_px: u32,
    x_px: i32,
    y_px: i32,
    font_size_px: f32,
    line_spacing: f32,
    threshold: u8,
    invert: bool,
    trim_blank_top_bottom: bool,
    density: u8,
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RenderTextResponse {
    render_id: String,
    width_px: u32,
    height_px: u32,
    preview_url: String,
}

#[derive(Debug, Serialize)]
struct RenderImageRequest {
    image_base64: String,
    width_px: u32,
    max_height_px: Option<u32>,
    threshold: u8,
    dither_method: DitherMethod,
    invert: bool,
    trim_blank_top_bottom: bool,
    density: u8,
    address: Option<String>,
}

#[derive(Debug, Serialize)]
struct AiGenerateRequest {
    prompt: String,
    size: String,
    quality: String,
    n: u8,
}

#[derive(Debug, Deserialize)]
struct AiGenerateResponse {
    image_base64: String,
    revised_prompt: Option<String>,
}

#[derive(Debug, Serialize)]
struct PrintRequest {
    render_id: String,
    address: Option<String>,
    density: u8,
}

#[derive(Debug, Deserialize)]
struct PrintResponse {
    job_id: String,
}

#[derive(Debug, Deserialize)]
struct JobResponse {
    status: String,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: String,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Команды:")]
enum Command {
    #[command(description = "помощь")]
    Help,
    #[command(description = "начало")]
    Start,
    #[command(description = "режим простого стикера")]
    Simple,
    #[command(description = "режим ИИ картинки")]
    Ai,
    #[command(description = "последние стикеры")]
    History,
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let args = Args::parse();
    let cfg_raw = tokio::fs::read_to_string(&args.config)
        .await
        .with_context(|| format!("failed to read config {}", args.config.display()))?;
    let cfg: Config = toml::from_str(&cfg_raw).context("failed to parse bot config")?;

    if cfg.sticker.density > 7 {
        bail!("sticker.density must be in 0..=7");
    }
    if cfg.image_sticker.density > 7 {
        bail!("image_sticker.density must be in 0..=7");
    }
    if cfg.sticker.printer_width_px == 0 {
        bail!("sticker.printer_width_px must be > 0");
    }

    let font_bytes = tokio::fs::read(&cfg.sticker.font_path)
        .await
        .with_context(|| format!("failed to read font {}", cfg.sticker.font_path))?;
    let font = FontArc::try_from_vec(font_bytes).context("failed to parse font")?;

    let db = Db::open(&cfg.sqlite_path).await?;
    db.init().await?;
    db.sync_allowlist(&cfg.access.allowed_user_ids).await?;

    let printerd = PrinterdClient::new(cfg.printerd.clone());
    let ai = AiServiceClient::new(cfg.ai_service.clone());

    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        db,
        printerd,
        ai,
        font,
        user_modes: Arc::new(RwLock::new(std::collections::HashMap::new())),
    });

    let bot = Bot::new(cfg.telegram_token);

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    let Some(user) = msg.from.as_ref() else {
        return Ok(());
    };
    let user_id = user.id.0 as i64;

    if !state.db.is_allowed(user_id).await.unwrap_or(false) {
        warn!(user_id = user_id, "telegram user denied by allowlist");
        bot.send_message(
            msg.chat.id,
            format!("Доступ пользователя {user_id} запрещён."),
        )
        .await?;
        return Ok(());
    }

    if let Some(text) = msg.text() {
        if let Some(cmd) = map_menu_button_to_command(text) {
            handle_command(&bot, &msg, &state, user_id, cmd).await?;
            return Ok(());
        }

        if let Ok(cmd) = Command::parse(text, "bot") {
            handle_command(&bot, &msg, &state, user_id, cmd).await?;
            return Ok(());
        }

        if text.starts_with('/') {
            bot.send_message(msg.chat.id, "Неизвестная команда. /help")
                .await?;
            return Ok(());
        }

        let mode = {
            let modes = state.user_modes.read().await;
            modes
                .get(&user_id)
                .copied()
                .unwrap_or(InputMode::SimpleText)
        };

        match mode {
            InputMode::SimpleText => {
                match create_simple_sticker(&state, user_id, msg.chat.id.0, text).await {
                    Ok(record) => {
                        info!(
                            user_id = user_id,
                            sticker_id = record.id,
                            "created text sticker preview"
                        );
                        let caption = format!(
                            "Превью стикера.\nШрифт: {:.1}px\nНажмите кнопку для печати.",
                            record.font_size_px
                        );
                        bot.send_photo(
                            msg.chat.id,
                            InputFile::memory(record.preview_png.clone()).file_name("preview.png"),
                        )
                        .caption(caption)
                        .reply_markup(print_keyboard(record.id))
                        .await?;
                    }
                    Err(err) => {
                        error!(user_id = user_id, error = %err, "failed to create text sticker preview");
                        bot.send_message(msg.chat.id, format!("Ошибка рендера: {err}"))
                            .await?;
                    }
                }
            }
            InputMode::AiImage => {
                let progress_msg = bot
                    .send_message(msg.chat.id, "Готовится изображение...")
                    .await
                    .ok();
                let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
                let bot_for_action = bot.clone();
                let chat_id = msg.chat.id;
                tokio::spawn(async move {
                    loop {
                        let _ = bot_for_action
                            .send_chat_action(chat_id, ChatAction::UploadPhoto)
                            .await;
                        tokio::select! {
                            _ = &mut stop_rx => break,
                            _ = tokio::time::sleep(Duration::from_secs(4)) => {}
                        }
                    }
                });

                match create_ai_image_sticker(&state, user_id, msg.chat.id.0, text).await {
                    Ok((record, revised_prompt)) => {
                        let _ = stop_tx.send(());
                        if let Some(progress_msg) = progress_msg {
                            let _ = bot.delete_message(msg.chat.id, progress_msg.id).await;
                        }
                        info!(
                            user_id = user_id,
                            sticker_id = record.id,
                            "created ai sticker preview"
                        );
                        let mut caption = String::from("Превью ИИ-изображения для печати.");
                        if let Some(rp) = revised_prompt {
                            caption.push_str("\nУточнённый промпт: ");
                            caption.push_str(&rp);
                        }
                        bot.send_photo(
                            msg.chat.id,
                            InputFile::memory(record.preview_png.clone()).file_name("preview.png"),
                        )
                        .caption(caption)
                        .reply_markup(print_keyboard(record.id))
                        .await?;
                    }
                    Err(err) => {
                        let _ = stop_tx.send(());
                        if let Some(progress_msg) = progress_msg {
                            let _ = bot.delete_message(msg.chat.id, progress_msg.id).await;
                        }
                        error!(user_id = user_id, error = %err, "failed to create ai sticker preview");
                        bot.send_message(msg.chat.id, format!("Ошибка AI генерации: {err}"))
                            .await?;
                    }
                }
            }
        }
        return Ok(());
    }

    if let Some(photos) = msg.photo() {
        if let Some(photo) = photos.last() {
            match create_image_sticker(&bot, &state, user_id, msg.chat.id.0, photo).await {
                Ok(record) => {
                    info!(
                        user_id = user_id,
                        sticker_id = record.id,
                        "created image sticker preview"
                    );
                    bot.send_photo(
                        msg.chat.id,
                        InputFile::memory(record.preview_png.clone()).file_name("preview.png"),
                    )
                    .caption("Превью изображения для печати.\nНажмите кнопку для печати.")
                    .reply_markup(print_keyboard(record.id))
                    .await?;
                }
                Err(err) => {
                    error!(user_id = user_id, error = %err, "failed to create image sticker preview");
                    bot.send_message(msg.chat.id, format!("Ошибка обработки изображения: {err}"))
                        .await?;
                }
            }
        }
    }

    Ok(())
}

async fn handle_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<AppState>,
    user_id: i64,
    cmd: Command,
) -> ResponseResult<()> {
    match cmd {
        Command::Help | Command::Start => {
            bot.send_message(
                msg.chat.id,
                "Режимы:\n• 🏷 Простой стикер: отправьте текст.\n• 🤖 ИИ картинка: отправьте описание изображения.\nТакже можно отправить готовую картинку.\nПосле превью нажмите Печатать.",
            )
            .reply_markup(main_menu_keyboard())
            .await?;
        }
        Command::Simple => {
            {
                let mut modes = state.user_modes.write().await;
                modes.insert(user_id, InputMode::SimpleText);
            }
            bot.send_message(
                msg.chat.id,
                "Режим: простой стикер. Просто отправьте текст следующим сообщением.",
            )
            .reply_markup(main_menu_keyboard())
            .await?;
        }
        Command::Ai => {
            {
                let mut modes = state.user_modes.write().await;
                modes.insert(user_id, InputMode::AiImage);
            }
            bot.send_message(
                msg.chat.id,
                "Режим: ИИ картинка. Отправьте текст-описание изображения, и я сгенерирую превью для печати.",
            )
            .reply_markup(main_menu_keyboard())
            .await?;
        }
        Command::History => match state.db.list_recent_for_user(user_id, 10).await {
            Ok(items) if items.is_empty() => {
                bot.send_message(msg.chat.id, "История пуста.")
                    .reply_markup(main_menu_keyboard())
                    .await?;
            }
            Ok(items) => {
                for item in items {
                    let caption = format!("{}\n{}", item.created_at, item.text);
                    bot.send_photo(
                        msg.chat.id,
                        InputFile::memory(item.preview_png.clone()).file_name("preview.png"),
                    )
                    .caption(caption)
                    .reply_markup(history_item_keyboard(item.id))
                    .await?;
                }
                bot.send_message(msg.chat.id, "Действия с историей:")
                    .reply_markup(clear_history_keyboard())
                    .await?;
            }
            Err(err) => {
                bot.send_message(msg.chat.id, format!("Ошибка чтения истории: {err}"))
                    .reply_markup(main_menu_keyboard())
                    .await?;
            }
        },
    }

    Ok(())
}

async fn handle_callback(bot: Bot, q: CallbackQuery, state: Arc<AppState>) -> ResponseResult<()> {
    let user_id = q.from.id.0 as i64;
    if !state.db.is_allowed(user_id).await.unwrap_or(false) {
        let _ = bot
            .answer_callback_query(q.id)
            .text("Доступ запрещён")
            .await;
        return Ok(());
    }

    let Some(data) = q.data.as_deref() else {
        return Ok(());
    };

    if data == "clear_history" {
        match state.db.clear_history_for_user(user_id).await {
            Ok(count) => {
                bot.answer_callback_query(q.id)
                    .text(format!("Удалено из истории: {count}"))
                    .await?;
            }
            Err(err) => {
                bot.answer_callback_query(q.id)
                    .show_alert(true)
                    .text(format!("Ошибка очистки: {err}"))
                    .await?;
            }
        }
        return Ok(());
    }

    let Some((action, id_str)) = data.split_once(':') else {
        return Ok(());
    };
    if action != "print" && action != "reprint" && action != "delete" {
        return Ok(());
    }

    let Ok(sticker_id) = id_str.parse::<i64>() else {
        return Ok(());
    };

    if action == "delete" {
        let result = state.db.delete_sticker_for_user(sticker_id, user_id).await;
        match result {
            Ok(true) => {
                bot.answer_callback_query(q.id.clone())
                    .text("Удалено из истории")
                    .await?;
                if let Some(message) = q.message {
                    let _ = bot
                        .edit_message_reply_markup(message.chat().id, message.id())
                        .reply_markup(InlineKeyboardMarkup::default())
                        .await;
                }
            }
            Ok(false) => {
                bot.answer_callback_query(q.id)
                    .show_alert(true)
                    .text("Не найдено")
                    .await?;
            }
            Err(err) => {
                bot.answer_callback_query(q.id)
                    .show_alert(true)
                    .text(format!("Ошибка удаления: {err}"))
                    .await?;
            }
        }
        return Ok(());
    }

    let result = process_print_action(&state, user_id, sticker_id).await;

    match result {
        Ok(job_id) => {
            bot.answer_callback_query(q.id.clone())
                .text(format!("Задание отправлено: {job_id}"))
                .await?;
            if let Some(message) = q.message {
                let _ = bot
                    .edit_message_reply_markup(message.chat().id, message.id())
                    .reply_markup(history_item_keyboard(sticker_id))
                    .await;
            }
        }
        Err(err) => {
            bot.answer_callback_query(q.id)
                .show_alert(true)
                .text(format!("Ошибка печати: {err}"))
                .await?;
        }
    }

    Ok(())
}

async fn create_simple_sticker(
    state: &AppState,
    user_id: i64,
    chat_id: i64,
    text: &str,
) -> Result<StickerRecord> {
    let cfg = &state.cfg.sticker;
    let content_width = cfg
        .printer_width_px
        .saturating_sub(cfg.margin_left_px)
        .saturating_sub(cfg.margin_right_px);
    if content_width < 16 {
        bail!("configured margins leave no content width");
    }

    let (font_size, text_height) = fit_font_size(
        &state.font,
        text,
        content_width as f32,
        cfg.min_font_size_px,
        cfg.max_font_size_px,
        cfg.line_spacing,
    )?;

    let height_px =
        (cfg.margin_top_px + cfg.margin_bottom_px + text_height.ceil() as u32 + 2).max(16);

    let req = RenderTextRequest {
        text: text.to_string(),
        font_path: cfg.font_path.clone(),
        width_px: cfg.printer_width_px,
        height_px,
        x_px: cfg.margin_left_px as i32,
        y_px: cfg.margin_top_px as i32,
        font_size_px: font_size,
        line_spacing: cfg.line_spacing,
        threshold: cfg.threshold,
        invert: cfg.invert,
        trim_blank_top_bottom: cfg.trim_blank_top_bottom,
        density: cfg.density,
        address: state.cfg.printerd.address.clone(),
    };

    let render = state.printerd.render_text(&req).await?;
    let preview_png = state.printerd.get_preview(&render.preview_url).await?;

    let id = state
        .db
        .insert_sticker(NewSticker {
            user_id,
            chat_id,
            kind: StickerKind::Text,
            text: text.to_string(),
            width_px: req.width_px,
            height_px: req.height_px,
            x_px: req.x_px,
            y_px: req.y_px,
            font_size_px: req.font_size_px,
            threshold: req.threshold,
            invert: req.invert,
            trim_blank_top_bottom: req.trim_blank_top_bottom,
            density: req.density,
            dither_method: None,
            source_image_bytes: None,
            preview_png: preview_png.clone(),
        })
        .await?;

    Ok(StickerRecord {
        id,
        kind: StickerKind::Text,
        text: text.to_string(),
        width_px: req.width_px,
        height_px: req.height_px,
        x_px: req.x_px,
        y_px: req.y_px,
        font_size_px: req.font_size_px,
        threshold: req.threshold,
        invert: req.invert,
        trim_blank_top_bottom: req.trim_blank_top_bottom,
        density: req.density,
        dither_method: None,
        source_image_bytes: None,
        preview_png,
        created_at: "now".to_string(),
    })
}

async fn create_image_sticker(
    bot: &Bot,
    state: &AppState,
    user_id: i64,
    chat_id: i64,
    photo: &teloxide::types::PhotoSize,
) -> Result<StickerRecord> {
    let file = bot
        .get_file(photo.file.id.clone())
        .await
        .context("failed to get telegram file metadata")?;
    let file_url = format!(
        "https://api.telegram.org/file/bot{}/{}",
        state.cfg.telegram_token, file.path
    );
    let bytes = reqwest::get(file_url)
        .await
        .context("failed to download telegram image")?
        .bytes()
        .await
        .context("failed to read telegram image body")?;
    create_image_sticker_from_bytes(state, user_id, chat_id, "Изображение", bytes.to_vec()).await
}

async fn create_ai_image_sticker(
    state: &AppState,
    user_id: i64,
    chat_id: i64,
    prompt: &str,
) -> Result<(StickerRecord, Option<String>)> {
    let ai_prompt = build_ai_lineart_prompt(prompt);
    let ai = state.ai.generate(&ai_prompt).await?;
    let source = base64::engine::general_purpose::STANDARD
        .decode(ai.image_base64.as_bytes())
        .context("ai-service returned invalid base64 image")?;
    let title = format!("AI: {prompt}");
    let image_cfg = &state.cfg.image_sticker;
    let ai_threshold = image_cfg.threshold.max(200);
    let sticker = create_image_sticker_from_bytes_with_options(
        state,
        user_id,
        chat_id,
        &title,
        source,
        ai_threshold,
        DitherMethod::Threshold,
        false,
    )
    .await?;
    Ok((sticker, ai.revised_prompt))
}

async fn create_image_sticker_from_bytes(
    state: &AppState,
    user_id: i64,
    chat_id: i64,
    title: &str,
    source: Vec<u8>,
) -> Result<StickerRecord> {
    let image_cfg = &state.cfg.image_sticker;
    create_image_sticker_from_bytes_with_options(
        state,
        user_id,
        chat_id,
        title,
        source,
        image_cfg.threshold,
        image_cfg.dither_method,
        image_cfg.invert,
    )
    .await
}

async fn create_image_sticker_from_bytes_with_options(
    state: &AppState,
    user_id: i64,
    chat_id: i64,
    title: &str,
    source: Vec<u8>,
    threshold: u8,
    dither_method: DitherMethod,
    invert: bool,
) -> Result<StickerRecord> {
    let image_cfg = &state.cfg.image_sticker;
    let req = RenderImageRequest {
        image_base64: base64::engine::general_purpose::STANDARD.encode(&source),
        width_px: state.cfg.sticker.printer_width_px,
        max_height_px: None,
        threshold,
        dither_method,
        invert,
        trim_blank_top_bottom: image_cfg.trim_blank_top_bottom,
        density: image_cfg.density,
        address: state.cfg.printerd.address.clone(),
    };

    let render = state.printerd.render_image(&req).await?;
    let preview_png = state.printerd.get_preview(&render.preview_url).await?;

    let id = state
        .db
        .insert_sticker(NewSticker {
            user_id,
            chat_id,
            kind: StickerKind::Image,
            text: title.to_string(),
            width_px: render.width_px,
            height_px: render.height_px,
            x_px: 0,
            y_px: 0,
            font_size_px: 0.0,
            threshold: req.threshold,
            invert: req.invert,
            trim_blank_top_bottom: req.trim_blank_top_bottom,
            density: req.density,
            dither_method: Some(req.dither_method),
            source_image_bytes: Some(source.clone()),
            preview_png: preview_png.clone(),
        })
        .await?;

    Ok(StickerRecord {
        id,
        kind: StickerKind::Image,
        text: title.to_string(),
        width_px: render.width_px,
        height_px: render.height_px,
        x_px: 0,
        y_px: 0,
        font_size_px: 0.0,
        threshold: req.threshold,
        invert: req.invert,
        trim_blank_top_bottom: req.trim_blank_top_bottom,
        density: req.density,
        dither_method: Some(req.dither_method),
        source_image_bytes: Some(source),
        preview_png,
        created_at: "now".to_string(),
    })
}

async fn process_print_action(state: &AppState, user_id: i64, sticker_id: i64) -> Result<String> {
    let Some(sticker) = state.db.get_sticker_for_user(sticker_id, user_id).await? else {
        bail!("стикер не найден");
    };

    let render = match sticker.kind {
        StickerKind::Text => {
            let req = RenderTextRequest {
                text: sticker.text.clone(),
                font_path: state.cfg.sticker.font_path.clone(),
                width_px: sticker.width_px,
                height_px: sticker.height_px,
                x_px: sticker.x_px,
                y_px: sticker.y_px,
                font_size_px: sticker.font_size_px,
                line_spacing: state.cfg.sticker.line_spacing,
                threshold: sticker.threshold,
                invert: sticker.invert,
                trim_blank_top_bottom: sticker.trim_blank_top_bottom,
                density: sticker.density,
                address: state.cfg.printerd.address.clone(),
            };
            state.printerd.render_text(&req).await?
        }
        StickerKind::Image => {
            let source = sticker
                .source_image_bytes
                .clone()
                .ok_or_else(|| anyhow!("missing source image in history"))?;
            let req = RenderImageRequest {
                image_base64: base64::engine::general_purpose::STANDARD.encode(source),
                width_px: sticker.width_px.max(1),
                max_height_px: Some(sticker.height_px.max(1)),
                threshold: sticker.threshold,
                dither_method: sticker
                    .dither_method
                    .unwrap_or(DitherMethod::FloydSteinberg),
                invert: sticker.invert,
                trim_blank_top_bottom: sticker.trim_blank_top_bottom,
                density: sticker.density,
                address: state.cfg.printerd.address.clone(),
            };
            state.printerd.render_image(&req).await?
        }
    };
    let print_resp = state
        .printerd
        .print_render(
            &render.render_id,
            sticker.density,
            state.cfg.printerd.address.clone(),
        )
        .await?;

    let wait_timeout = state.cfg.printerd.wait_job_timeout_seconds.unwrap_or(20);
    let job = state
        .printerd
        .wait_job(&print_resp.job_id, wait_timeout)
        .await?;
    if job.status == "failed" {
        bail!(
            "принтер вернул ошибку: {}",
            job.error.unwrap_or_else(|| "unknown".to_string())
        );
    }
    if job.status != "done" {
        bail!("печать не завершилась вовремя, статус: {}", job.status);
    }

    state
        .db
        .set_last_print_job(sticker_id, &print_resp.job_id)
        .await?;

    info!(
        user_id = user_id,
        sticker_id = sticker_id,
        job_id = %print_resp.job_id,
        "sticker printed"
    );

    Ok(print_resp.job_id)
}

fn fit_font_size(
    font: &FontArc,
    text: &str,
    max_width: f32,
    min_size: f32,
    max_size: f32,
    line_spacing: f32,
) -> Result<(f32, f32)> {
    if min_size <= 0.0 || max_size <= 0.0 || min_size > max_size {
        bail!("invalid font size bounds");
    }

    let mut lo = min_size;
    let mut hi = max_size;

    let (min_w, min_h) = measure_text_block(font, text, min_size, line_spacing);
    if min_w > max_width {
        bail!("text is too wide even at minimum font size {:.1}", min_size);
    }

    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        let (w, _) = measure_text_block(font, text, mid, line_spacing);
        if w <= max_width {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let (_, h) = measure_text_block(font, text, lo, line_spacing);
    Ok((lo, h.max(min_h)))
}

fn build_ai_lineart_prompt(user_prompt: &str) -> String {
    format!(
        "Create black ink line art for thermal sticker printing. \
Pure white background. Thin clean outlines. \
No shading, no gray tones, no gradients, no fill textures, no color, no text. \
Centered composition with clear silhouette. Subject: {}",
        user_prompt
    )
}

fn measure_text_block(font: &FontArc, text: &str, font_size: f32, line_spacing: f32) -> (f32, f32) {
    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);

    let lines: Vec<&str> = text.split('\n').collect();
    let mut max_width = 0.0f32;

    for line in &lines {
        let mut width = 0.0f32;
        let mut prev = None;
        for ch in line.chars() {
            let gid = scaled.glyph_id(ch);
            if let Some(pg) = prev {
                width += scaled.kern(pg, gid);
            }
            width += scaled.h_advance(gid);
            prev = Some(gid);
        }
        if width > max_width {
            max_width = width;
        }
    }

    let line_h = (scaled.ascent() - scaled.descent() + scaled.line_gap()).max(1.0) * line_spacing;
    let total_h = line_h * lines.len().max(1) as f32;

    (max_width, total_h)
}

fn print_keyboard(sticker_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "Печатать",
        format!("print:{sticker_id}"),
    )]])
}

fn history_item_keyboard(sticker_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "Напечатать ещё раз",
            format!("reprint:{sticker_id}"),
        )],
        vec![InlineKeyboardButton::callback(
            "Удалить из истории",
            format!("delete:{sticker_id}"),
        )],
    ])
}

fn clear_history_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "Очистить всю историю",
        "clear_history",
    )]])
}

fn main_menu_keyboard() -> KeyboardMarkup {
    KeyboardMarkup::new(vec![
        vec![
            KeyboardButton::new("🆘 Помощь"),
            KeyboardButton::new("🗂 История"),
        ],
        vec![
            KeyboardButton::new("🏷 Простой стикер"),
            KeyboardButton::new("🤖 ИИ картинка"),
        ],
    ])
    .resize_keyboard()
}

fn map_menu_button_to_command(text: &str) -> Option<Command> {
    match text.trim() {
        "🆘 Помощь" => Some(Command::Help),
        "🗂 История" => Some(Command::History),
        "🏷 Простой стикер" => Some(Command::Simple),
        "🤖 ИИ картинка" => Some(Command::Ai),
        _ => None,
    }
}

fn parse_kind(kind: String) -> StickerKind {
    if kind.eq_ignore_ascii_case("image") {
        StickerKind::Image
    } else {
        StickerKind::Text
    }
}

fn parse_dither_opt(v: Option<String>) -> Option<DitherMethod> {
    match v.as_deref() {
        Some("threshold") => Some(DitherMethod::Threshold),
        Some("floyd_steinberg") => Some(DitherMethod::FloydSteinberg),
        _ => None,
    }
}

impl PrinterdClient {
    fn new(cfg: PrinterdConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            token: cfg.api_token,
            default_address: cfg.address,
        }
    }

    async fn render_text(&self, req: &RenderTextRequest) -> Result<RenderTextResponse> {
        let url = format!("{}/api/v1/renders/text", self.base_url);
        let mut request = self.http.post(url).json(req);
        if let Some(token) = &self.token {
            request = request.header("x-api-token", token);
        }
        let resp = request.send().await.context("printerd request failed")?;
        parse_json_response(resp).await
    }

    async fn render_image(&self, req: &RenderImageRequest) -> Result<RenderTextResponse> {
        let url = format!("{}/api/v1/renders/image", self.base_url);
        let mut request = self.http.post(url).json(req);
        if let Some(token) = &self.token {
            request = request.header("x-api-token", token);
        }
        let resp = request
            .send()
            .await
            .context("printerd image request failed")?;
        parse_json_response(resp).await
    }

    async fn get_preview(&self, preview_url: &str) -> Result<Vec<u8>> {
        let url = if preview_url.starts_with("http://") || preview_url.starts_with("https://") {
            preview_url.to_string()
        } else {
            format!("{}{}", self.base_url, preview_url)
        };

        let mut request = self.http.get(url);
        if let Some(token) = &self.token {
            request = request.header("x-api-token", token);
        }
        let resp = request.send().await.context("preview request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("preview request failed with {status}: {body}");
        }
        let bytes = resp.bytes().await.context("failed to read preview body")?;
        Ok(bytes.to_vec())
    }

    async fn print_render(
        &self,
        render_id: &str,
        density: u8,
        address: Option<String>,
    ) -> Result<PrintResponse> {
        let url = format!("{}/api/v1/print", self.base_url);
        let req = PrintRequest {
            render_id: render_id.to_string(),
            address: address.or_else(|| self.default_address.clone()),
            density,
        };

        let mut request = self.http.post(url).json(&req);
        if let Some(token) = &self.token {
            request = request.header("x-api-token", token);
        }
        let resp = request.send().await.context("print request failed")?;
        parse_json_response(resp).await
    }

    async fn wait_job(&self, job_id: &str, timeout_seconds: u64) -> Result<JobResponse> {
        let url = format!(
            "{}/api/v1/jobs/{}/wait?timeout_seconds={}",
            self.base_url,
            job_id,
            timeout_seconds.clamp(1, 120)
        );
        let mut request = self.http.get(url);
        if let Some(token) = &self.token {
            request = request.header("x-api-token", token);
        }
        let resp = request.send().await.context("wait job request failed")?;
        parse_json_response(resp).await
    }
}

impl AiServiceClient {
    fn new(cfg: AiServiceConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            token: cfg.api_token,
            default_size: cfg.default_size.unwrap_or_else(|| "1024x1024".to_string()),
            default_quality: cfg.default_quality.unwrap_or_else(|| "low".to_string()),
        }
    }

    async fn generate(&self, prompt: &str) -> Result<AiGenerateResponse> {
        let req = AiGenerateRequest {
            prompt: prompt.to_string(),
            size: self.default_size.clone(),
            quality: self.default_quality.clone(),
            n: 1,
        };
        let mut request = self
            .http
            .post(format!("{}/api/v1/generate", self.base_url))
            .json(&req);
        if let Some(token) = &self.token {
            request = request.header("x-api-token", token);
        }
        let resp = request.send().await.context("ai-service request failed")?;
        parse_json_response(resp).await
    }
}

async fn parse_json_response<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        return resp
            .json::<T>()
            .await
            .context("failed to decode printerd json response");
    }

    let text = resp.text().await.unwrap_or_default();
    if let Ok(err_body) = serde_json::from_str::<ApiErrorBody>(&text) {
        bail!("printerd error {}: {}", status, err_body.error);
    }
    bail!("printerd error {}: {}", status, text)
}

struct NewSticker {
    user_id: i64,
    chat_id: i64,
    kind: StickerKind,
    text: String,
    width_px: u32,
    height_px: u32,
    x_px: i32,
    y_px: i32,
    font_size_px: f32,
    threshold: u8,
    invert: bool,
    trim_blank_top_bottom: bool,
    density: u8,
    dither_method: Option<DitherMethod>,
    source_image_bytes: Option<Vec<u8>>,
    preview_png: Vec<u8>,
}

impl Db {
    async fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)
            .await
            .with_context(|| format!("failed to open sqlite db {path}"))?;
        Ok(Self {
            conn: Arc::new(conn),
        })
    }

    async fn init(&self) -> Result<()> {
        self.conn
            .call(|conn| -> rusqlite::Result<()> {
                conn.execute_batch(
                    "
                    PRAGMA journal_mode = WAL;
                    CREATE TABLE IF NOT EXISTS allowed_users (
                        user_id INTEGER PRIMARY KEY,
                        note TEXT,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                    );
                    CREATE TABLE IF NOT EXISTS stickers (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        user_id INTEGER NOT NULL,
                        chat_id INTEGER NOT NULL,
                        kind TEXT NOT NULL DEFAULT 'text',
                        text TEXT NOT NULL,
                        width_px INTEGER NOT NULL,
                        height_px INTEGER NOT NULL,
                        x_px INTEGER NOT NULL,
                        y_px INTEGER NOT NULL,
                        font_size_px REAL NOT NULL,
                        threshold INTEGER NOT NULL,
                        invert INTEGER NOT NULL,
                        trim_blank_top_bottom INTEGER NOT NULL,
                        density INTEGER NOT NULL,
                        dither_method TEXT,
                        source_image_bytes BLOB,
                        preview_png BLOB NOT NULL,
                        last_printer_job_id TEXT,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                    );
                    CREATE INDEX IF NOT EXISTS idx_stickers_user_created ON stickers(user_id, id DESC);
                    ",
                )?;
                // Migrations for existing DBs.
                let _ = conn.execute("ALTER TABLE stickers ADD COLUMN kind TEXT NOT NULL DEFAULT 'text'", []);
                let _ = conn.execute("ALTER TABLE stickers ADD COLUMN dither_method TEXT", []);
                let _ = conn.execute("ALTER TABLE stickers ADD COLUMN source_image_bytes BLOB", []);
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("failed to initialize sqlite schema: {e}"))?;
        Ok(())
    }

    async fn sync_allowlist(&self, user_ids: &[i64]) -> Result<()> {
        let ids = user_ids.to_vec();
        self.conn
            .call(move |conn| -> rusqlite::Result<()> {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "INSERT OR IGNORE INTO allowed_users (user_id, note) VALUES (?1, 'from config')",
                    )?;
                    for uid in ids {
                        stmt.execute([uid])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("failed to sync allowlist: {e}"))?;
        Ok(())
    }

    async fn is_allowed(&self, user_id: i64) -> Result<bool> {
        self.conn
            .call(move |conn| -> rusqlite::Result<bool> {
                let exists: i64 = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM allowed_users WHERE user_id = ?1)",
                    [user_id],
                    |row| row.get(0),
                )?;
                Ok(exists == 1)
            })
            .await
            .map_err(|e| anyhow!("failed to check allowlist: {e}"))
    }

    async fn insert_sticker(&self, s: NewSticker) -> Result<i64> {
        self.conn
            .call(move |conn| -> rusqlite::Result<i64> {
                conn.execute(
                    "INSERT INTO stickers (
                        user_id, chat_id, kind, text, width_px, height_px, x_px, y_px,
                        font_size_px, threshold, invert, trim_blank_top_bottom,
                        density, dither_method, source_image_bytes, preview_png
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                    (
                        s.user_id,
                        s.chat_id,
                        match s.kind {
                            StickerKind::Text => "text",
                            StickerKind::Image => "image",
                        },
                        s.text,
                        s.width_px as i64,
                        s.height_px as i64,
                        s.x_px,
                        s.y_px,
                        s.font_size_px,
                        s.threshold as i64,
                        if s.invert { 1 } else { 0 },
                        if s.trim_blank_top_bottom { 1 } else { 0 },
                        s.density as i64,
                        s.dither_method.map(|m| match m {
                            DitherMethod::Threshold => "threshold",
                            DitherMethod::FloydSteinberg => "floyd_steinberg",
                        }),
                        s.source_image_bytes,
                        s.preview_png,
                    ),
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await
            .map_err(|e| anyhow!("failed to insert sticker: {e}"))
    }

    async fn get_sticker_for_user(&self, id: i64, user_id: i64) -> Result<Option<StickerRecord>> {
        self.conn
            .call(move |conn| -> rusqlite::Result<Option<StickerRecord>> {
                let mut stmt = conn.prepare(
                    "SELECT id, kind, text, width_px, height_px, x_px, y_px, font_size_px,
                            threshold, invert, trim_blank_top_bottom, density, dither_method, source_image_bytes, preview_png, created_at
                     FROM stickers
                     WHERE id = ?1 AND user_id = ?2",
                )?;

                let mut rows = stmt.query((id, user_id))?;
                let Some(row) = rows.next()? else {
                    return Ok(None);
                };

                Ok(Some(StickerRecord {
                    id: row.get(0)?,
                    kind: parse_kind(row.get::<_, String>(1)?),
                    text: row.get(2)?,
                    width_px: row.get::<_, i64>(3)? as u32,
                    height_px: row.get::<_, i64>(4)? as u32,
                    x_px: row.get(5)?,
                    y_px: row.get(6)?,
                    font_size_px: row.get(7)?,
                    threshold: row.get::<_, i64>(8)? as u8,
                    invert: row.get::<_, i64>(9)? != 0,
                    trim_blank_top_bottom: row.get::<_, i64>(10)? != 0,
                    density: row.get::<_, i64>(11)? as u8,
                    dither_method: parse_dither_opt(row.get::<_, Option<String>>(12)?),
                    source_image_bytes: row.get(13)?,
                    preview_png: row.get(14)?,
                    created_at: row.get(15)?,
                }))
            })
            .await
            .map_err(|e| anyhow!("failed to load sticker: {e}"))
    }

    async fn list_recent_for_user(&self, user_id: i64, limit: i64) -> Result<Vec<StickerRecord>> {
        self.conn
            .call(move |conn| -> rusqlite::Result<Vec<StickerRecord>> {
                let mut stmt = conn.prepare(
                    "SELECT id, kind, text, width_px, height_px, x_px, y_px, font_size_px,
                            threshold, invert, trim_blank_top_bottom, density, dither_method, source_image_bytes, preview_png, created_at
                     FROM stickers
                     WHERE user_id = ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;

                let rows = stmt.query_map((user_id, limit), |row| {
                    Ok(StickerRecord {
                        id: row.get(0)?,
                        kind: parse_kind(row.get::<_, String>(1)?),
                        text: row.get(2)?,
                        width_px: row.get::<_, i64>(3)? as u32,
                        height_px: row.get::<_, i64>(4)? as u32,
                        x_px: row.get(5)?,
                        y_px: row.get(6)?,
                        font_size_px: row.get(7)?,
                        threshold: row.get::<_, i64>(8)? as u8,
                        invert: row.get::<_, i64>(9)? != 0,
                        trim_blank_top_bottom: row.get::<_, i64>(10)? != 0,
                        density: row.get::<_, i64>(11)? as u8,
                        dither_method: parse_dither_opt(row.get::<_, Option<String>>(12)?),
                        source_image_bytes: row.get(13)?,
                        preview_png: row.get(14)?,
                        created_at: row.get(15)?,
                    })
                })?;

                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .await
            .map_err(|e| anyhow!("failed to load history: {e}"))
    }

    async fn set_last_print_job(&self, id: i64, job_id: &str) -> Result<()> {
        let jid = job_id.to_string();
        self.conn
            .call(move |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE stickers SET last_printer_job_id = ?1 WHERE id = ?2",
                    (jid, id),
                )?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("failed to update print job id: {e}"))
    }

    async fn delete_sticker_for_user(&self, id: i64, user_id: i64) -> Result<bool> {
        self.conn
            .call(move |conn| -> rusqlite::Result<bool> {
                let changed = conn.execute(
                    "DELETE FROM stickers WHERE id = ?1 AND user_id = ?2",
                    (id, user_id),
                )?;
                Ok(changed > 0)
            })
            .await
            .map_err(|e| anyhow!("failed to delete history item: {e}"))
    }

    async fn clear_history_for_user(&self, user_id: i64) -> Result<u64> {
        self.conn
            .call(move |conn| -> rusqlite::Result<u64> {
                let changed = conn.execute("DELETE FROM stickers WHERE user_id = ?1", [user_id])?;
                Ok(changed as u64)
            })
            .await
            .map_err(|e| anyhow!("failed to clear history: {e}"))
    }
}
