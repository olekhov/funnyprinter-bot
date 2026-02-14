use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use funnyprint_proto::{MAX_DOTS_PER_LINE, discover_candidates, dpi, print_job};
use funnyprint_render::{TextRenderOptions, image_to_packed_lines, px_to_mm, render_text_to_image};

#[derive(Debug, Parser)]
#[command(name = "funnyprint")]
#[command(about = "Direct BLE printing for FunnyPrint/Xiqi printers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Scan {
        #[arg(long, default_value_t = 2)]
        seconds: u64,
    },
    PrintText {
        #[arg(long)]
        address: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        font: PathBuf,
        #[arg(long, default_value_t = 48.0)]
        font_size: f32,
        #[arg(long, default_value_t = 0)]
        x: i32,
        #[arg(long, default_value_t = 0)]
        y: i32,
        #[arg(long, default_value_t = MAX_DOTS_PER_LINE as u32)]
        width: u32,
        #[arg(long, default_value_t = 192)]
        height: u32,
        #[arg(long, default_value_t = 180)]
        threshold: u8,
        #[arg(long, default_value_t = 3)]
        density: u8,
        #[arg(long, default_value = "preview.png")]
        preview: PathBuf,
        #[arg(long, default_value_t = false)]
        invert: bool,
        #[arg(long, default_value_t = false)]
        no_trim_blank: bool,
        #[arg(long, default_value_t = false)]
        preview_only: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Scan { seconds } => {
            let found = discover_candidates(Duration::from_secs(seconds)).await?;
            if found.is_empty() {
                println!("No candidate devices found");
            } else {
                for p in found {
                    println!(
                        "{}\t{}",
                        p.address,
                        p.local_name.unwrap_or_else(|| "<unknown>".to_string())
                    );
                }
            }
        }
        Command::PrintText {
            address,
            text,
            font,
            font_size,
            x,
            y,
            width,
            height,
            threshold,
            density,
            preview,
            invert,
            no_trim_blank,
            preview_only,
        } => {
            if width as usize > MAX_DOTS_PER_LINE {
                bail!(
                    "width {} exceeds printer max {} dots ({} dpi)",
                    width,
                    MAX_DOTS_PER_LINE,
                    dpi()
                );
            }

            let opts = TextRenderOptions {
                width_px: width,
                height_px: height,
                x_px: x,
                y_px: y,
                font_size_px: font_size,
                threshold,
                invert,
                trim_blank_top_bottom: !no_trim_blank,
            };

            let img = render_text_to_image(&text, &font, &opts)?;
            img.save(&preview)
                .with_context(|| format!("failed to save preview PNG to {}", preview.display()))?;

            let packed = image_to_packed_lines(&img, threshold, opts.trim_blank_top_bottom);
            println!(
                "Preview saved: {} ({}x{} px, {:.2}x{:.2} mm at {} dpi, {} packed lines)",
                preview.display(),
                img.width(),
                img.height(),
                px_to_mm(img.width(), dpi()),
                px_to_mm(img.height(), dpi()),
                dpi(),
                packed.len()
            );

            if preview_only {
                return Ok(());
            }

            if packed.is_empty() {
                bail!("image became empty after trimming blank lines; nothing to print")
            }

            print_job(&address, &packed, density).await?;
            println!("Print job sent to {}", address);
        }
    }

    Ok(())
}
