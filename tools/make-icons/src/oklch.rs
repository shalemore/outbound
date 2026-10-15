//! Colors are chosen in OKLCH, where equal numbers look equally light and equally colorful, and
//! turned into sRGB only when an image is rendered.

use anyhow::{Context, Result, bail};

/// Replaces every `oklch(L C H)` in the text with the `#rrggbb` that is closest to it.
pub fn convert_all(text: &str) -> Result<String> {
    const START: &str = "oklch(";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(START) {
        out.push_str(&rest[..at]);
        let after = &rest[at + START.len()..];
        let end = after.find(')').context("an oklch( has no closing )")?;
        let [l, c, h] = parse(&after[..end])?;
        let [r, g, b] = to_srgb(l, c, h);
        out.push_str(&format!("#{r:02X}{g:02X}{b:02X}"));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn parse(arguments: &str) -> Result<[f64; 3]> {
    let numbers: Vec<f64> = arguments
        .split_whitespace()
        .map(|word| {
            word.parse()
                .with_context(|| format!("not a number: {word}"))
        })
        .collect::<Result<_>>()?;
    match numbers.as_slice() {
        &[l, c, h] if (0.0..=1.0).contains(&l) && c >= 0.0 => Ok([l, c, h]),
        _ => bail!("expected oklch(lightness 0..1, chroma, hue in degrees), got {arguments:?}"),
    }
}

/// OKLCH to 8 bit sRGB. A color outside the sRGB gamut keeps its lightness and hue and loses
/// chroma until it fits.
pub fn to_srgb(l: f64, c: f64, h: f64) -> [u8; 3] {
    let fits = |c: f64| {
        linear(l, c, h)
            .iter()
            .all(|v| (-1e-4..=1.0 + 1e-4).contains(v))
    };
    let mut chroma = c;
    if !fits(c) {
        let (mut low, mut high) = (0.0, c);
        for _ in 0..30 {
            let middle = (low + high) / 2.0;
            if fits(middle) {
                low = middle;
            } else {
                high = middle;
            }
        }
        chroma = low;
    }
    linear(l, chroma, h).map(|v| (gamma(v.clamp(0.0, 1.0)) * 255.0).round() as u8)
}

fn linear(l: f64, c: f64, h: f64) -> [f64; 3] {
    let (a, b) = (c * h.to_radians().cos(), c * h.to_radians().sin());
    let l_ = (l + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m_ = (l - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s_ = (l - 0.0894841775 * a - 1.2914855480 * b).powi(3);
    [
        4.0767416621 * l_ - 3.3077115913 * m_ + 0.2309699292 * s_,
        -1.2684380046 * l_ + 2.6097574011 * m_ - 0.3413193965 * s_,
        -0.0041960863 * l_ - 0.7034186147 * m_ + 1.7076147010 * s_,
    ]
}

fn gamma(value: f64) -> f64 {
    if value <= 0.003_130_8 {
        12.92 * value
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: [u8; 3], expected: [u8; 3]) {
        for (a, e) in actual.iter().zip(expected) {
            assert!(
                a.abs_diff(e) <= 2,
                "{actual:?} is not close to {expected:?}"
            );
        }
    }

    #[test]
    fn known_colors() {
        assert_eq!(to_srgb(1.0, 0.0, 0.0), [255, 255, 255]);
        assert_eq!(to_srgb(0.0, 0.0, 0.0), [0, 0, 0]);
        // The sRGB primary red, and a mid gray (#636363).
        close(to_srgb(0.627_955, 0.257_683, 29.233_9), [255, 0, 0]);
        close(to_srgb(0.5, 0.0, 0.0), [0x63, 0x63, 0x63]);
    }

    #[test]
    fn out_of_gamut_colors_keep_their_lightness() {
        // Far more chroma than sRGB can show.
        let [r, g, b] = to_srgb(0.6, 0.5, 145.0);
        assert!(g > r && g > b, "still a green: {r} {g} {b}");
    }

    #[test]
    fn converts_every_color_in_a_text() {
        let text = r#"<a fill="oklch(1 0 0)"/><b stroke="oklch(0 0 0)"/>"#;
        assert_eq!(
            convert_all(text).unwrap(),
            r##"<a fill="#FFFFFF"/><b stroke="#000000"/>"##
        );
        assert_eq!(convert_all("no colors").unwrap(), "no colors");
    }

    #[test]
    fn rejects_malformed_colors() {
        assert!(convert_all("oklch(1 0)").is_err());
        assert!(convert_all("oklch(2 0 0)").is_err());
        assert!(convert_all("oklch(1 0 0").is_err());
        assert!(convert_all("oklch(a b c)").is_err());
    }
}
