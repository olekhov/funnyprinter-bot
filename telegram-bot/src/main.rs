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
    #[serde(default)]
    allowed_user_ids: Vec<i64>,
    #[serde(default)]
    admin_user_ids: Vec<i64>,
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
    OutlineText,
    Banner,
    BannerOutline,
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
    TextOutline,
    TextBanner,
    TextBannerOutline,
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
    outline_only: bool,
    outline_thickness_px: u32,
    banner_mode: bool,
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
    model: String,
    size: String,
    quality: String,
    usage: Option<AiUsage>,
}

#[derive(Debug, Deserialize, Clone)]
struct AiUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
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
    #[command(description = "режим контурного текста")]
    Outline,
    #[command(description = "режим баннера")]
    Banner,
    #[command(description = "режим баннера контуром")]
    BannerOutline,
    #[command(description = "режим ИИ картинки")]
    Ai,
    #[command(description = "последние стикеры")]
    History,
    #[command(description = "статистика AI и пользователей")]
    Stats,
    #[command(description = "список пользователей (admin)")]
    Users,
    #[command(description = "добавить пользователя: /user_add <telegram_user_id> (admin)")]
    UserAdd(String),
    #[command(description = "удалить пользователя: /user_del <telegram_user_id> (admin)")]
    UserDel(String),
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
    let admin_ids = if cfg.access.admin_user_ids.is_empty() {
        cfg.access.allowed_user_ids.clone()
    } else {
        cfg.access.admin_user_ids.clone()
    };
    db.sync_users(&cfg.access.allowed_user_ids, &admin_ids).await?;

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
                match create_text_sticker(
                    &state,
                    user_id,
                    msg.chat.id.0,
                    text,
                    StickerKind::Text,
                )
                .await
                {
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
            InputMode::OutlineText => {
                match create_text_sticker(
                    &state,
                    user_id,
                    msg.chat.id.0,
                    text,
                    StickerKind::TextOutline,
                )
                .await
                {
                    Ok(record) => {
                        info!(user_id = user_id, sticker_id = record.id, "created outline text preview");
                        bot.send_photo(
                            msg.chat.id,
                            InputFile::memory(record.preview_png.clone()).file_name("preview.png"),
                        )
                        .caption("Превью контурного текста.\nНажмите кнопку для печати.")
                        .reply_markup(print_keyboard(record.id))
                        .await?;
                    }
                    Err(err) => {
                        error!(user_id = user_id, error = %err, "failed to create outline text preview");
                        bot.send_message(msg.chat.id, format!("Ошибка рендера: {err}"))
                            .await?;
                    }
                }
            }
            InputMode::Banner => {
                match create_text_sticker(
                    &state,
                    user_id,
                    msg.chat.id.0,
                    text,
                    StickerKind::TextBanner,
                )
                .await
                {
                    Ok(record) => {
                        info!(user_id = user_id, sticker_id = record.id, "created banner preview");
                        bot.send_photo(
                            msg.chat.id,
                            InputFile::memory(record.preview_png.clone()).file_name("preview.png"),
                        )
                        .caption("Превью баннера.\nНажмите кнопку для печати.")
                        .reply_markup(print_keyboard(record.id))
                        .await?;
                    }
                    Err(err) => {
                        error!(user_id = user_id, error = %err, "failed to create banner preview");
                        bot.send_message(msg.chat.id, format!("Ошибка рендера: {err}"))
                            .await?;
                    }
                }
            }
            InputMode::BannerOutline => {
                match create_text_sticker(
                    &state,
                    user_id,
                    msg.chat.id.0,
                    text,
                    StickerKind::TextBannerOutline,
                )
                .await
                {
                    Ok(record) => {
                        info!(user_id = user_id, sticker_id = record.id, "created banner outline preview");
                        bot.send_photo(
                            msg.chat.id,
                            InputFile::memory(record.preview_png.clone()).file_name("preview.png"),
                        )
                        .caption("Превью баннера (контур).\nНажмите кнопку для печати.")
                        .reply_markup(print_keyboard(record.id))
                        .await?;
                    }
                    Err(err) => {
                        error!(user_id = user_id, error = %err, "failed to create banner outline preview");
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
                        let _ = state
                            .db
                            .insert_ai_generation(NewAiGeneration {
                                user_id,
                                chat_id: msg.chat.id.0,
                                prompt: text.to_string(),
                                revised_prompt: None,
                                model: None,
                                size: None,
                                quality: None,
                                input_tokens: None,
                                output_tokens: None,
                                total_tokens: None,
                                status: "error".to_string(),
                                error: Some(err.to_string()),
                            })
                            .await;
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
    let is_admin = state.db.is_admin(user_id).await.unwrap_or(false);

    match cmd {
        Command::Help | Command::Start => {
            bot.send_message(
                msg.chat.id,
                "Режимы:\n• 🏷 Простой стикер: отправьте текст.\n• ✏️ Контур текста: буквы без заливки.\n• 🧾 Баннер: печать вдоль ленты.\n• 🧾✏️ Баннер контуром.\n• 🤖 ИИ картинка: отправьте описание изображения.\nТакже можно отправить готовую картинку.\n• 📊 Статистика: пользователи и токены AI.\nПосле превью нажмите Печатать.",
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
        Command::Outline => {
            {
                let mut modes = state.user_modes.write().await;
                modes.insert(user_id, InputMode::OutlineText);
            }
            bot.send_message(
                msg.chat.id,
                "Режим: контур текста. Отправьте текст следующим сообщением.",
            )
            .reply_markup(main_menu_keyboard())
            .await?;
        }
        Command::Banner => {
            {
                let mut modes = state.user_modes.write().await;
                modes.insert(user_id, InputMode::Banner);
            }
            bot.send_message(
                msg.chat.id,
                "Режим: баннер. Текст печатается вдоль ленты.",
            )
            .reply_markup(main_menu_keyboard())
            .await?;
        }
        Command::BannerOutline => {
            {
                let mut modes = state.user_modes.write().await;
                modes.insert(user_id, InputMode::BannerOutline);
            }
            bot.send_message(
                msg.chat.id,
                "Режим: баннер контуром. Текст вдоль ленты и без заливки.",
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
        Command::Stats => match state.db.ai_stats().await {
            Ok(stats) => {
                let mut text = format!(
                    "Статистика:\nПользователей в allowlist: {}\nAI генераций: {}\nAI токенов: {} (in: {}, out: {})",
                    stats.allowed_users_count,
                    stats.ai_generation_count,
                    stats.total_tokens,
                    stats.input_tokens,
                    stats.output_tokens
                );
                if !stats.by_user.is_empty() {
                    text.push_str("\n\nТоп по токенам:");
                    for row in stats.by_user.iter().take(10) {
                        text.push_str(&format!(
                            "\n• {}: {} токенов, {} генераций",
                            row.user_id, row.total_tokens, row.generation_count
                        ));
                    }
                }
                bot.send_message(msg.chat.id, text)
                    .reply_markup(main_menu_keyboard())
                    .await?;
            }
            Err(err) => {
                bot.send_message(msg.chat.id, format!("Ошибка статистики: {err}"))
                    .reply_markup(main_menu_keyboard())
                    .await?;
            }
        },
        Command::Users => {
            if !is_admin {
                bot.send_message(msg.chat.id, "Команда доступна только администратору.")
                    .await?;
                return Ok(());
            }
            match state.db.list_users().await {
                Ok(users) if users.is_empty() => {
                    bot.send_message(msg.chat.id, "Список пользователей пуст.")
                        .await?;
                }
                Ok(users) => {
                    let mut text = String::from("Пользователи:");
                    for u in users {
                        let role = if u.is_admin { "admin" } else { "user" };
                        text.push_str(&format!("\n• {} [{}] {}", u.user_id, role, u.note));
                    }
                    bot.send_message(msg.chat.id, text).await?;
                }
                Err(err) => {
                    bot.send_message(msg.chat.id, format!("Ошибка списка пользователей: {err}"))
                        .await?;
                }
            }
        }
        Command::UserAdd(arg) => {
            if !is_admin {
                bot.send_message(msg.chat.id, "Команда доступна только администратору.")
                    .await?;
                return Ok(());
            }
            let Ok(target_user_id) = arg.trim().parse::<i64>() else {
                bot.send_message(msg.chat.id, "Формат: /user_add <telegram_user_id>")
                    .await?;
                return Ok(());
            };
            let note = format!("added by admin {}", user_id);
            match state.db.upsert_user(target_user_id, &note, false).await {
                Ok(()) => {
                    bot.send_message(msg.chat.id, format!("Пользователь {target_user_id} добавлен."))
                        .await?;
                }
                Err(err) => {
                    bot.send_message(msg.chat.id, format!("Ошибка добавления: {err}"))
                        .await?;
                }
            }
        }
        Command::UserDel(arg) => {
            if !is_admin {
                bot.send_message(msg.chat.id, "Команда доступна только администратору.")
                    .await?;
                return Ok(());
            }
            let Ok(target_user_id) = arg.trim().parse::<i64>() else {
                bot.send_message(msg.chat.id, "Формат: /user_del <telegram_user_id>")
                    .await?;
                return Ok(());
            };
            match state.db.delete_user(target_user_id).await {
                Ok(true) => {
                    bot.send_message(msg.chat.id, format!("Пользователь {target_user_id} удалён."))
                        .await?;
                }
                Ok(false) => {
                    bot.send_message(msg.chat.id, "Пользователь не найден.")
                        .await?;
                }
                Err(err) => {
                    bot.send_message(msg.chat.id, format!("Ошибка удаления: {err}"))
                        .await?;
                }
            }
        }
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

async fn create_text_sticker(
    state: &AppState,
    user_id: i64,
    chat_id: i64,
    text: &str,
    kind: StickerKind,
) -> Result<StickerRecord> {
    let cfg = &state.cfg.sticker;
    let is_banner = matches!(kind, StickerKind::TextBanner | StickerKind::TextBannerOutline);
    let outline_only = matches!(kind, StickerKind::TextOutline | StickerKind::TextBannerOutline);

    let (width_px, height_px, x_px, y_px, font_size) = if is_banner {
        let content_height = cfg
            .printer_width_px
            .saturating_sub(cfg.margin_top_px)
            .saturating_sub(cfg.margin_bottom_px);
        if content_height < 12 {
            bail!("configured margins leave no content height for banner mode");
        }
        let (font_size, _) = fit_font_size_by_height(
            &state.font,
            text,
            content_height as f32,
            cfg.min_font_size_px,
            cfg.max_font_size_px,
            cfg.line_spacing,
        )?;
        let (text_width, text_height) = measure_text_block(&state.font, text, font_size, cfg.line_spacing);
        let width_px = (cfg.margin_left_px + cfg.margin_right_px + text_width.ceil() as u32 + 2).max(16);
        let y_px = cfg.margin_top_px as i32
            + ((content_height as i32 - text_height.ceil() as i32).max(0) / 2);
        (
            width_px,
            cfg.printer_width_px,
            cfg.margin_left_px as i32,
            y_px,
            font_size,
        )
    } else {
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
        (
            cfg.printer_width_px,
            height_px,
            cfg.margin_left_px as i32,
            cfg.margin_top_px as i32,
            font_size,
        )
    };

    let req = RenderTextRequest {
        text: text.to_string(),
        font_path: cfg.font_path.clone(),
        width_px,
        height_px,
        x_px,
        y_px,
        font_size_px: font_size,
        line_spacing: cfg.line_spacing,
        threshold: cfg.threshold,
        invert: cfg.invert,
        trim_blank_top_bottom: cfg.trim_blank_top_bottom,
        outline_only,
        outline_thickness_px: 1,
        banner_mode: is_banner,
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
            kind,
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
        kind,
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
    state
        .db
        .insert_ai_generation(NewAiGeneration {
            user_id,
            chat_id,
            prompt: prompt.to_string(),
            revised_prompt: ai.revised_prompt.clone(),
            model: Some(ai.model.clone()),
            size: Some(ai.size.clone()),
            quality: Some(ai.quality.clone()),
            input_tokens: ai.usage.as_ref().and_then(|u| u.input_tokens),
            output_tokens: ai.usage.as_ref().and_then(|u| u.output_tokens),
            total_tokens: ai.usage.as_ref().and_then(|u| u.total_tokens),
            status: "ok".to_string(),
            error: None,
        })
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
        StickerKind::Text
        | StickerKind::TextOutline
        | StickerKind::TextBanner
        | StickerKind::TextBannerOutline => {
            let outline_only = matches!(
                sticker.kind,
                StickerKind::TextOutline | StickerKind::TextBannerOutline
            );
            let banner_mode = matches!(
                sticker.kind,
                StickerKind::TextBanner | StickerKind::TextBannerOutline
            );
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
                outline_only,
                outline_thickness_px: 1,
                banner_mode,
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

fn fit_font_size_by_height(
    font: &FontArc,
    text: &str,
    max_height: f32,
    min_size: f32,
    max_size: f32,
    line_spacing: f32,
) -> Result<(f32, f32)> {
    if min_size <= 0.0 || max_size <= 0.0 || min_size > max_size {
        bail!("invalid font size bounds");
    }

    let (_, min_h) = measure_text_block(font, text, min_size, line_spacing);
    if min_h > max_height {
        bail!("text is too tall even at minimum font size {:.1}", min_size);
    }

    let mut lo = min_size;
    let mut hi = max_size;
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        let (_, h) = measure_text_block(font, text, mid, line_spacing);
        if h <= max_height {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let (_, h) = measure_text_block(font, text, lo, line_spacing);
    Ok((lo, h))
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
            KeyboardButton::new("📊 Статистика"),
        ],
        vec![
            KeyboardButton::new("🏷 Простой стикер"),
            KeyboardButton::new("✏️ Контур текста"),
        ],
        vec![
            KeyboardButton::new("🧾 Баннер"),
            KeyboardButton::new("🧾✏️ Баннер контуром"),
        ],
        vec![
            KeyboardButton::new("🤖 ИИ картинка"),
        ],
    ])
    .resize_keyboard()
}

fn map_menu_button_to_command(text: &str) -> Option<Command> {
    match text.trim() {
        "🆘 Помощь" => Some(Command::Help),
        "🗂 История" => Some(Command::History),
        "📊 Статистика" => Some(Command::Stats),
        "🏷 Простой стикер" => Some(Command::Simple),
        "✏️ Контур текста" => Some(Command::Outline),
        "🧾 Баннер" => Some(Command::Banner),
        "🧾✏️ Баннер контуром" => Some(Command::BannerOutline),
        "🤖 ИИ картинка" => Some(Command::Ai),
        _ => None,
    }
}

fn parse_kind(kind: String) -> StickerKind {
    match kind.as_str() {
        "image" => StickerKind::Image,
        "text_outline" => StickerKind::TextOutline,
        "text_banner" => StickerKind::TextBanner,
        "text_banner_outline" => StickerKind::TextBannerOutline,
        _ => StickerKind::Text,
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

struct NewAiGeneration {
    user_id: i64,
    chat_id: i64,
    prompt: String,
    revised_prompt: Option<String>,
    model: Option<String>,
    size: Option<String>,
    quality: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    status: String,
    error: Option<String>,
}

struct AiStatsSummary {
    allowed_users_count: u64,
    ai_generation_count: u64,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    by_user: Vec<AiStatsByUser>,
}

struct AiStatsByUser {
    user_id: i64,
    generation_count: u64,
    total_tokens: u64,
}

struct AllowedUser {
    user_id: i64,
    is_admin: bool,
    note: String,
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
                        is_admin INTEGER NOT NULL DEFAULT 0,
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
                    CREATE TABLE IF NOT EXISTS ai_generations (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        user_id INTEGER NOT NULL,
                        chat_id INTEGER NOT NULL,
                        prompt TEXT NOT NULL,
                        revised_prompt TEXT,
                        model TEXT,
                        size TEXT,
                        quality TEXT,
                        input_tokens INTEGER,
                        output_tokens INTEGER,
                        total_tokens INTEGER,
                        status TEXT NOT NULL,
                        error TEXT,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                    );
                    CREATE INDEX IF NOT EXISTS idx_ai_generations_user_created ON ai_generations(user_id, id DESC);
                    ",
                )?;
                // Migrations for existing DBs.
                let _ = conn.execute(
                    "ALTER TABLE allowed_users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0",
                    [],
                );
                let _ = conn.execute("ALTER TABLE stickers ADD COLUMN kind TEXT NOT NULL DEFAULT 'text'", []);
                let _ = conn.execute("ALTER TABLE stickers ADD COLUMN dither_method TEXT", []);
                let _ = conn.execute("ALTER TABLE stickers ADD COLUMN source_image_bytes BLOB", []);
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("failed to initialize sqlite schema: {e}"))?;
        Ok(())
    }

    async fn sync_users(&self, user_ids: &[i64], admin_ids: &[i64]) -> Result<()> {
        let ids = user_ids.to_vec();
        let admins = admin_ids.to_vec();
        self.conn
            .call(move |conn| -> rusqlite::Result<()> {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "INSERT INTO allowed_users (user_id, is_admin, note)
                         VALUES (?1, 0, 'from config')
                         ON CONFLICT(user_id) DO UPDATE SET note = excluded.note",
                    )?;
                    for uid in ids {
                        stmt.execute([uid])?;
                    }
                }
                {
                    let mut stmt = tx.prepare(
                        "INSERT INTO allowed_users (user_id, is_admin, note)
                         VALUES (?1, 1, 'admin from config')
                         ON CONFLICT(user_id) DO UPDATE SET is_admin = 1, note = excluded.note",
                    )?;
                    for uid in admins {
                        stmt.execute([uid])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("failed to sync users: {e}"))?;
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

    async fn is_admin(&self, user_id: i64) -> Result<bool> {
        self.conn
            .call(move |conn| -> rusqlite::Result<bool> {
                let exists: i64 = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM allowed_users WHERE user_id = ?1 AND is_admin = 1)",
                    [user_id],
                    |row| row.get(0),
                )?;
                Ok(exists == 1)
            })
            .await
            .map_err(|e| anyhow!("failed to check admin role: {e}"))
    }

    async fn upsert_user(&self, user_id: i64, note: &str, is_admin: bool) -> Result<()> {
        let note = note.to_string();
        self.conn
            .call(move |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "INSERT INTO allowed_users (user_id, is_admin, note)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(user_id) DO UPDATE SET is_admin = excluded.is_admin, note = excluded.note",
                    (user_id, if is_admin { 1 } else { 0 }, note),
                )?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("failed to upsert user: {e}"))
    }

    async fn delete_user(&self, user_id: i64) -> Result<bool> {
        self.conn
            .call(move |conn| -> rusqlite::Result<bool> {
                let changed = conn.execute("DELETE FROM allowed_users WHERE user_id = ?1", [user_id])?;
                Ok(changed > 0)
            })
            .await
            .map_err(|e| anyhow!("failed to delete user: {e}"))
    }

    async fn list_users(&self) -> Result<Vec<AllowedUser>> {
        self.conn
            .call(move |conn| -> rusqlite::Result<Vec<AllowedUser>> {
                let mut stmt = conn.prepare(
                    "SELECT user_id, is_admin, COALESCE(note, '')
                     FROM allowed_users
                     ORDER BY is_admin DESC, user_id ASC",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(AllowedUser {
                        user_id: row.get(0)?,
                        is_admin: row.get::<_, i64>(1)? != 0,
                        note: row.get(2)?,
                    })
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .await
            .map_err(|e| anyhow!("failed to list users: {e}"))
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
                            StickerKind::TextOutline => "text_outline",
                            StickerKind::TextBanner => "text_banner",
                            StickerKind::TextBannerOutline => "text_banner_outline",
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

    async fn insert_ai_generation(&self, g: NewAiGeneration) -> Result<i64> {
        self.conn
            .call(move |conn| -> rusqlite::Result<i64> {
                conn.execute(
                    "INSERT INTO ai_generations (
                        user_id, chat_id, prompt, revised_prompt, model, size, quality,
                        input_tokens, output_tokens, total_tokens, status, error
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    (
                        g.user_id,
                        g.chat_id,
                        g.prompt,
                        g.revised_prompt,
                        g.model,
                        g.size,
                        g.quality,
                        g.input_tokens.map(|v| v as i64),
                        g.output_tokens.map(|v| v as i64),
                        g.total_tokens.map(|v| v as i64),
                        g.status,
                        g.error,
                    ),
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await
            .map_err(|e| anyhow!("failed to insert ai generation: {e}"))
    }

    async fn ai_stats(&self) -> Result<AiStatsSummary> {
        self.conn
            .call(move |conn| -> rusqlite::Result<AiStatsSummary> {
                let allowed_users_count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM allowed_users", [], |row| row.get(0))?;
                let (ai_generation_count, input_tokens, output_tokens, total_tokens): (
                    i64,
                    i64,
                    i64,
                    i64,
                ) = conn.query_row(
                    "SELECT
                        COUNT(*),
                        COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(total_tokens), 0)
                     FROM ai_generations
                     WHERE status = 'ok'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;

                let mut stmt = conn.prepare(
                    "SELECT user_id, COUNT(*) AS cnt, COALESCE(SUM(total_tokens), 0) AS tokens
                     FROM ai_generations
                     WHERE status = 'ok'
                     GROUP BY user_id
                     ORDER BY tokens DESC, cnt DESC
                     LIMIT 20",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(AiStatsByUser {
                        user_id: row.get(0)?,
                        generation_count: row.get::<_, i64>(1)? as u64,
                        total_tokens: row.get::<_, i64>(2)? as u64,
                    })
                })?;
                let mut by_user = Vec::new();
                for row in rows {
                    by_user.push(row?);
                }

                Ok(AiStatsSummary {
                    allowed_users_count: allowed_users_count as u64,
                    ai_generation_count: ai_generation_count as u64,
                    input_tokens: input_tokens as u64,
                    output_tokens: output_tokens as u64,
                    total_tokens: total_tokens as u64,
                    by_user,
                })
            })
            .await
            .map_err(|e| anyhow!("failed to get ai stats: {e}"))
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
