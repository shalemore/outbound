use anyhow::Result;
use tray_icon::Icon;

use crate::status::IconState;

const SIZE: u32 = 32;
/// The progress bar lies in columns 3..29 and rows 25..30 of the icon, in the room that the
/// images made by tools/make-icons leave free.
const BAR_COLUMNS: std::ops::Range<u32> = 3..29;
const BAR_ROWS: std::ops::Range<u32> = 25..30;

const IDLE: &[u8] = include_bytes!("../assets/tray/idle.rgba");
const BUSY: &[u8] = include_bytes!("../assets/tray/busy.rgba");
const WARNING: &[u8] = include_bytes!("../assets/tray/warning.rgba");
const ERROR: &[u8] = include_bytes!("../assets/tray/error.rgba");
const PAUSED: &[u8] = include_bytes!("../assets/tray/paused.rgba");

/// The tray icon: an image per state, with the progress bar drawn over its bottom edge.
pub fn render(state: IconState, progress: Option<f32>) -> Result<Icon> {
    Ok(Icon::from_rgba(rgba(state, progress), SIZE, SIZE)?)
}

fn rgba(state: IconState, progress: Option<f32>) -> Vec<u8> {
    let mut rgba = match state {
        IconState::Idle => IDLE,
        IconState::Busy => BUSY,
        IconState::Warning => WARNING,
        IconState::Error => ERROR,
        IconState::Paused => PAUSED,
    }
    .to_vec();
    if let Some(progress) = progress {
        draw_progress(&mut rgba, progress);
    }
    rgba
}

fn draw_progress(rgba: &mut [u8], progress: f32) {
    let width = BAR_COLUMNS.len() as f32;
    let filled = (progress.clamp(0.0, 1.0) * width).round() as u32;
    for y in BAR_ROWS {
        for x in BAR_COLUMNS {
            let start = ((y * SIZE + x) * 4) as usize;
            let pixel = &mut rgba[start..start + 4];
            if x - BAR_COLUMNS.start < filled {
                pixel[..3].fill(255);
            } else {
                // The empty part of the bar is the background, darkened.
                for channel in &mut pixel[..3] {
                    *channel = (u16::from(*channel) * 57 / 100) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [IconState; 5] = [
        IconState::Idle,
        IconState::Busy,
        IconState::Warning,
        IconState::Error,
        IconState::Paused,
    ];

    #[test]
    fn every_state_has_a_full_image() {
        for state in STATES {
            assert_eq!(rgba(state, None).len(), (SIZE * SIZE * 4) as usize);
            render(state, Some(0.5)).unwrap();
        }
    }

    fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
        let start = ((y * SIZE + x) * 4) as usize;
        rgba[start..start + 4].try_into().unwrap()
    }

    #[test]
    fn the_corner_is_transparent_and_the_center_is_not() {
        let image = rgba(IconState::Idle, None);
        assert_eq!(pixel(&image, 0, 0)[3], 0);
        assert_eq!(pixel(&image, 16, 8)[3], 255);
    }

    #[test]
    fn the_bar_fills_from_the_left() {
        let image = rgba(IconState::Busy, Some(0.5));
        assert_eq!(&pixel(&image, 4, 27)[..3], [255, 255, 255], "filled part");
        let empty = pixel(&image, 27, 27);
        assert!(empty[0] < 255 && empty[3] == 255, "empty part is darkened");
        let untouched = rgba(IconState::Busy, None);
        assert_eq!(
            pixel(&image, 16, 8),
            pixel(&untouched, 16, 8),
            "nothing else changes"
        );
    }
}
