# printerbot / funnyprint rust workspace

Rust workspace for direct BLE printing to FunnyPrint-compatible printers (Xiqi/DOLEWA class), without CUPS.

Protocol reversed by [ValdikSS](https://github.com/ValdikSS/printer-driver-funnyprint/tree/master).

## Crates

- `funnyprint-proto`: BLE protocol and printer interaction logic ported from `printer-driver-funnyprint` Python driver.
- `funnyprint-render`: text-to-image rendering and conversion into printer packed lines.
- `funnyprint-cli`: CLI for scanning BLE printers and printing text with PNG preview output.
- `printerd`: HTTP daemon with render cache, preview endpoint and queued print jobs.
- `telegram-bot`: Telegram UI over `printerd` with confirm-print flow and persistent history in SQLite.

## Driver-derived printer facts

From `printer-driver-funnyprint/xiqi.drv` and ValdikSS's Python code:

- Resolution: `203 dpi`
- Max print width in protocol packets: `384 dots` (48 bytes per raster row)
- Width sanity check used by driver: up to `390 px` in CUPS raster input, then trimmed to 384 for transport.
- Media presets in driver include: `58x999mm`, `48x999mm`, `58x60mm`, `58x40mm`, `58x30mm`, `52x34mm`, `40x58mm`.

## CLI usage

Scan for nearby candidates:

```bash
cargo run -p funnyprint-cli -- scan --seconds 3
```

Render text + preview PNG + print:

```bash
cargo run -p funnyprint-cli -- print-text \
  --address C0:00:00:00:05:AB \
  --text "Hello sticker" \
  --font /path/to/font.ttf \
  --font-size 48 \
  --x 8 --y 16 \
  --width 384 --height 192 \
  --threshold 180 \
  --density 3 \
  --preview preview.png
```

Preview only (without sending to printer):

```bash
cargo run -p funnyprint-cli -- print-text \
  --address C0:00:00:00:05:AB \
  --text "Preview" \
  --font /path/to/font.ttf \
  --preview-only
```

## printerd (LAN-ready HTTP daemon)

Start daemon (bind all interfaces):

```bash
cargo run -p printerd -- \
  --listen 0.0.0.0:8080 \
  --default-address C0:00:00:00:06:B3
```
Structured logs with tracing:
```bash
RUST_LOG=info cargo run -p printerd -- --listen 0.0.0.0:8080 --default-address C0:00:00:00:06:B3
```

Optional auth token:

```bash
cargo run -p printerd -- \
  --listen 0.0.0.0:8080 \
  --default-address C0:00:00:00:06:B3 \
  --api-token change-me
```
When token is set, include `-H 'x-api-token: change-me'` in all `/api/v1/*` requests.

Main flow:

1. Render text and get `render_id`:
```bash
curl -sS -X POST http://<pi-ip>:8080/api/v1/renders/text \
  -H 'content-type: application/json' \
  -d '{
    "text":"Hello sticker",
    "font_path":"/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "font_size_px":48,
    "x_px":8,
    "y_px":16
  }'
```

2. Show preview:
```bash
curl -sS http://<pi-ip>:8080/api/v1/renders/r_1/preview > preview.png
```

3. Queue print:
```bash
curl -sS -X POST http://<pi-ip>:8080/api/v1/print \
  -H 'content-type: application/json' \
  -d '{"render_id":"r_1"}'
```

4. Check job status:
```bash
curl -sS http://<pi-ip>:8080/api/v1/jobs/j_1
```

5. Wait for completion/failure (useful for bot feedback):
```bash
curl -sS "http://<pi-ip>:8080/api/v1/jobs/j_1/wait?timeout_seconds=20"
```

## Telegram Bot

The bot uses `printerd` as rendering/printing backend and keeps history in SQLite, so previews and reprint buttons survive bot restarts.

### Run

```bash
cp telegram-bot/bot-config.example.toml bot-config.toml
$EDITOR bot-config.toml
cargo run -p telegram-bot -- --config bot-config.toml
```
Tracing logs:
```bash
RUST_LOG=info cargo run -p telegram-bot -- --config bot-config.toml
```

### Simple Sticker flow

1. Allowed user sends multi-line text to the bot.
2. Bot calculates the largest fitting font size for configured margins and width.
3. Bot requests preview from `printerd`, stores sticker record in SQLite, sends preview image.
4. User presses `Печатать`.
5. Bot re-renders by saved parameters and sends print request.
6. Button becomes `Напечатать ещё раз` for quick reprint.
7. Bot shows menu buttons (`Помощь`, `История`, `Простой стикер`) as reply keyboard.

### Access control

Only users from `allowed_users` SQLite table can use the bot.

- Initial seeding is done from `access.allowed_user_ids` in config.
- To add/remove manually on Raspberry Pi:

```bash
sqlite3 printerbot.sqlite3 "INSERT OR IGNORE INTO allowed_users (user_id, note) VALUES (123456789, 'manual');"
sqlite3 printerbot.sqlite3 "DELETE FROM allowed_users WHERE user_id = 123456789;"
```

To get your Telegram user id, send `/start` to `@userinfobot` (or similar bot), then add that id.

### History actions

- Each history preview has:
  - `Напечатать ещё раз`
  - `Удалить из истории`
- History screen also has `Очистить всю историю` (only for current user history).
