//! Renders the icon assets of outbound from `assets/icon.svg`.
//!
//! ```text
//! cargo run --manifest-path tools/make-icons/Cargo.toml -- build assets
//! cargo run --manifest-path tools/make-icons/Cargo.toml -- preview sheet.png a.svg b.svg
//! ```
//!
//! `build` writes `icon.ico` and `icon-256.png` for the executable and the notifications, and one
//! raw RGBA image per tray state in `tray/`. The tray images get the state color as background and
//! a smaller glyph, which leaves room for the progress bar that the program draws at the bottom.
//! `preview` draws the given icons at several sizes into one picture, to compare designs.

use std::{env, fs, path::Path};

mod oklch;

use anyhow::{Context, Result, bail};
use resvg::{
    tiny_skia::{Color, Pixmap, PixmapPaint, Transform},
    usvg,
};

/// The brand color. icon.svg has to use exactly this text for its background.
const BRAND: &str = "oklch(0.33 0.055 215)";
const GLYPH: &str = r#"<g id="glyph">"#;
const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
const TRAY_SIZE: u32 = 32;
/// Scale and vertical offset of the glyph in the tray images, in the 256 unit icon space.
const TRAY_GLYPH: &str = r#"<g id="glyph" transform="translate(25.6 2) scale(0.8)">"#;

/// The tray states and the background color of each, in OKLCH (lightness, chroma, hue). The four
/// states that are not the brand color share one lightness, so that none of them stands out and
/// the white glyph is equally readable on all of them; only chroma and hue tell them apart.
const TRAY_STATES: [(&str, &str); 5] = [
    ("idle", BRAND),
    ("busy", "oklch(0.56 0.16 255)"),
    ("warning", "oklch(0.62 0.13 80)"),
    ("error", "oklch(0.55 0.19 27)"),
    ("paused", "oklch(0.56 0.02 255)"),
];

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["build", assets] => build(Path::new(assets)),
        ["preview", out, svgs @ ..] if !svgs.is_empty() => preview(Path::new(out), svgs),
        _ => bail!("usage: make-icons build <assets dir> | preview <out.png> <icon.svg>..."),
    }
}

fn render(svg: &str, size: u32) -> Result<Pixmap> {
    let svg = oklch::convert_all(svg)?;
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default())?;
    let mut pixmap = Pixmap::new(size, size).context("invalid icon size")?;
    let scale = size as f32 / tree.size().width();
    resvg::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

fn build(assets: &Path) -> Result<()> {
    let svg = fs::read_to_string(assets.join("icon.svg")).context("cannot read icon.svg")?;

    let mut images = Vec::new();
    for size in ICO_SIZES {
        images.push((size, render(&svg, size)?.encode_png()?));
    }
    fs::write(assets.join("icon.ico"), ico(&images))?;
    fs::write(
        assets.join("icon-256.png"),
        &images.last().context("no sizes")?.1,
    )?;

    let tray = assets.join("tray");
    fs::create_dir_all(&tray)?;
    for (state, color) in TRAY_STATES {
        let svg = tray_variant(&svg, color)?;
        let rgba = straight_rgba(&render(&svg, TRAY_SIZE)?);
        fs::write(tray.join(format!("{state}.rgba")), rgba)?;
    }
    println!(
        "wrote icon.ico, icon-256.png and {} tray images",
        TRAY_STATES.len()
    );
    Ok(())
}

fn tray_variant(svg: &str, color: &str) -> Result<String> {
    let background = format!(r#"fill="{BRAND}""#);
    if !svg.contains(&background) || !svg.contains(GLYPH) {
        bail!("icon.svg must contain {background} and {GLYPH}");
    }
    Ok(svg
        .replace(&background, &format!(r#"fill="{color}""#))
        .replace(GLYPH, TRAY_GLYPH))
}

/// tiny-skia keeps premultiplied alpha, the tray wants straight alpha.
fn straight_rgba(pixmap: &Pixmap) -> Vec<u8> {
    pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect()
}

/// An ICO file whose images are PNG files, which Windows has supported since Vista.
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let count = u16::try_from(images.len()).expect("few images");
    let mut file = vec![0, 0, 1, 0];
    file.extend(count.to_le_bytes());

    let mut offset = 6 + 16 * u32::from(count);
    for (size, png) in images {
        // A size of 256 is stored as 0.
        let byte = if *size >= 256 { 0 } else { *size as u8 };
        file.extend([byte, byte, 0, 0]);
        file.extend(1u16.to_le_bytes()); // color planes
        file.extend(32u16.to_le_bytes()); // bits per pixel
        file.extend(u32::try_from(png.len()).expect("small image").to_le_bytes());
        file.extend(offset.to_le_bytes());
        offset += u32::try_from(png.len()).expect("small image");
    }
    for (_, png) in images {
        file.extend(png);
    }
    file
}

fn preview(out: &Path, svgs: &[&str]) -> Result<()> {
    const SIZES: [u32; 4] = [256, 64, 32, 16];
    const GAP: u32 = 16;
    let width = SIZES.iter().sum::<u32>() + GAP * (SIZES.len() as u32 + 1);
    let height = (256 + GAP) * svgs.len() as u32 + GAP;
    let mut sheet = Pixmap::new(width, height).context("sheet size")?;
    sheet.fill(Color::from_rgba8(236, 238, 240, 255));

    for (row, path) in svgs.iter().enumerate() {
        let svg = fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
        let mut x = GAP;
        for size in SIZES {
            let icon = render(&svg, size)?;
            let y = GAP + (256 + GAP) * row as u32;
            sheet.draw_pixmap(
                x as i32,
                y as i32,
                icon.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            x += size + GAP;
        }
    }
    fs::write(out, sheet.encode_png()?)?;
    Ok(())
}
