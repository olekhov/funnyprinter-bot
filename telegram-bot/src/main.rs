use std::{path::PathBuf, sync::Arc};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};
use teloxide::{
    dispatching::UpdateFilterExt,
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile},
    utils::command::BotCommands,
};
use tokio_rusqlite::{Connection, rusqlite};

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
    sticker: StickerConfig,
    access: AccessConfig,
}

#[derive(Debug, Clone, Deserialize)]
struct PrinterdConfig {
    base_url: String,
    api_token: Option<String>,
    address: Option<String>,
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
struct AccessConfig {
    allowed_user_ids: Vec<i64>,
}

#[derive(Clone)]
struct AppState {
    cfg: Config,
    db: Db,
    printerd: PrinterdClient,
    font: FontArc,
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

#[derive(Debug, Clone)]
struct StickerRecord {
    id: i64,
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
    preview_png: Vec<u8>,
    created_at: String,
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
    preview_url: String,
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
    #[command(description = "последние стикеры")]
    History,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg_raw = tokio::fs::read_to_string(&args.config)
        .await
        .with_context(|| format!("failed to read config {}", args.config.display()))?;
    let cfg: Config = toml::from_str(&cfg_raw).context("failed to parse bot config")?;

    if cfg.sticker.density > 7 {
        bail!("sticker.density must be in 0..=7");
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

    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        db,
        printerd,
        font,
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
        bot.send_message(
            msg.chat.id,
            format!("Доступ пользователя {user_id} запрещён."),
        )
        .await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };

    if let Ok(cmd) = Command::parse(text, "bot") {
        handle_command(&bot, &msg, &state, user_id, cmd).await?;
        return Ok(());
    }

    if text.starts_with('/') {
        bot.send_message(msg.chat.id, "Неизвестная команда. /help")
            .await?;
        return Ok(());
    }

    match create_simple_sticker(&state, user_id, msg.chat.id.0, text).await {
        Ok(record) => {
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
            bot.send_message(msg.chat.id, format!("Ошибка рендера: {err}"))
                .await?;
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
                "Отправьте текст (поддерживаются переносы строк) и бот пришлёт превью.\\nПосле подтверждения стикер будет отправлен на печать.",
            )
            .await?;
        }
        Command::Simple => {
            bot.send_message(
                msg.chat.id,
                "Режим: простой стикер. Просто отправьте текст следующим сообщением.",
            )
            .await?;
        }
        Command::History => match state.db.list_recent_for_user(user_id, 10).await {
            Ok(items) if items.is_empty() => {
                bot.send_message(msg.chat.id, "История пуста.").await?;
            }
            Ok(items) => {
                for item in items {
                    let caption = format!("{}\n{}", item.created_at, item.text);
                    bot.send_photo(
                        msg.chat.id,
                        InputFile::memory(item.preview_png.clone()).file_name("preview.png"),
                    )
                    .caption(caption)
                    .reply_markup(reprint_keyboard(item.id))
                    .await?;
                }
            }
            Err(err) => {
                bot.send_message(msg.chat.id, format!("Ошибка чтения истории: {err}"))
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

    let Some((action, id_str)) = data.split_once(':') else {
        return Ok(());
    };
    if action != "print" && action != "reprint" {
        return Ok(());
    }

    let Ok(sticker_id) = id_str.parse::<i64>() else {
        return Ok(());
    };

    let result = process_print_action(&state, user_id, sticker_id).await;

    match result {
        Ok(job_id) => {
            bot.answer_callback_query(q.id.clone())
                .text(format!("Задание отправлено: {job_id}"))
                .await?;
            if let Some(message) = q.message {
                let _ = bot
                    .edit_message_reply_markup(message.chat().id, message.id())
                    .reply_markup(reprint_keyboard(sticker_id))
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
            preview_png: preview_png.clone(),
        })
        .await?;

    Ok(StickerRecord {
        id,
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
        preview_png,
        created_at: "now".to_string(),
    })
}

async fn process_print_action(state: &AppState, user_id: i64, sticker_id: i64) -> Result<String> {
    let Some(sticker) = state.db.get_sticker_for_user(sticker_id, user_id).await? else {
        bail!("стикер не найден");
    };

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

    let render = state.printerd.render_text(&req).await?;
    let print_resp = state
        .printerd
        .print_render(
            &render.render_id,
            req.density,
            state.cfg.printerd.address.clone(),
        )
        .await?;

    state
        .db
        .set_last_print_job(sticker_id, &print_resp.job_id)
        .await?;

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

fn reprint_keyboard(sticker_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "Напечатать ещё раз",
        format!("reprint:{sticker_id}"),
    )]])
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
                        preview_png BLOB NOT NULL,
                        last_printer_job_id TEXT,
                        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                    );
                    CREATE INDEX IF NOT EXISTS idx_stickers_user_created ON stickers(user_id, id DESC);
                    ",
                )?;
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
                        user_id, chat_id, text, width_px, height_px, x_px, y_px,
                        font_size_px, threshold, invert, trim_blank_top_bottom,
                        density, preview_png
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    (
                        s.user_id,
                        s.chat_id,
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
                    "SELECT id, text, width_px, height_px, x_px, y_px, font_size_px,
                            threshold, invert, trim_blank_top_bottom, density, preview_png, created_at
                     FROM stickers
                     WHERE id = ?1 AND user_id = ?2",
                )?;

                let mut rows = stmt.query((id, user_id))?;
                let Some(row) = rows.next()? else {
                    return Ok(None);
                };

                Ok(Some(StickerRecord {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    width_px: row.get::<_, i64>(2)? as u32,
                    height_px: row.get::<_, i64>(3)? as u32,
                    x_px: row.get(4)?,
                    y_px: row.get(5)?,
                    font_size_px: row.get(6)?,
                    threshold: row.get::<_, i64>(7)? as u8,
                    invert: row.get::<_, i64>(8)? != 0,
                    trim_blank_top_bottom: row.get::<_, i64>(9)? != 0,
                    density: row.get::<_, i64>(10)? as u8,
                    preview_png: row.get(11)?,
                    created_at: row.get(12)?,
                }))
            })
            .await
            .map_err(|e| anyhow!("failed to load sticker: {e}"))
    }

    async fn list_recent_for_user(&self, user_id: i64, limit: i64) -> Result<Vec<StickerRecord>> {
        self.conn
            .call(move |conn| -> rusqlite::Result<Vec<StickerRecord>> {
                let mut stmt = conn.prepare(
                    "SELECT id, text, width_px, height_px, x_px, y_px, font_size_px,
                            threshold, invert, trim_blank_top_bottom, density, preview_png, created_at
                     FROM stickers
                     WHERE user_id = ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;

                let rows = stmt.query_map((user_id, limit), |row| {
                    Ok(StickerRecord {
                        id: row.get(0)?,
                        text: row.get(1)?,
                        width_px: row.get::<_, i64>(2)? as u32,
                        height_px: row.get::<_, i64>(3)? as u32,
                        x_px: row.get(4)?,
                        y_px: row.get(5)?,
                        font_size_px: row.get(6)?,
                        threshold: row.get::<_, i64>(7)? as u8,
                        invert: row.get::<_, i64>(8)? != 0,
                        trim_blank_top_bottom: row.get::<_, i64>(9)? != 0,
                        density: row.get::<_, i64>(10)? as u8,
                        preview_png: row.get(11)?,
                        created_at: row.get(12)?,
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
}
