# printerbot / funnyprint rust workspace

Rust workspace for direct BLE printing to FunnyPrint-compatible printers (Xiqi/DOLEWA class), without CUPS.

## Crates

- `funnyprint-proto`: BLE protocol and printer interaction logic ported from `printer-driver-funnyprint` Python driver.
- `funnyprint-render`: text-to-image rendering and conversion into printer packed lines.
- `funnyprint-cli`: CLI for scanning BLE printers and printing text with PNG preview output.

## Driver-derived printer facts

From `../printer-driver-funnyprint/xiqi.drv` and Python code:

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
