//! Shared OLED renderer used by both keyboard halves.
//!
//! Referenced from `keyboard.toml` as
//! `renderer = "crate::display::renderer::DOIORenderer"` for both
//! `[split.central.display]` and `[split.peripheral.display]`.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};

use embedded_graphics::image::{Image, ImageRaw};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};
use rmk::display::{DisplayRenderer, RenderContext};
use rmk::types::battery::BatteryStatus;
use rmk::types::ble::BleState;

use super::icons;

/// Whether the keyboard is currently charging. Inferred from USB presence: the
/// central sets it via [`set_charging`] on connection changes, and
/// [`draw_battery_status`] reads it to draw the charging icon. A shared global
/// is needed because renderers only see [`RenderContext`], which carries no USB
/// state. Only the central updates this; the peripheral has no host USB so it
/// stays `false` there.
static CHARGING: AtomicBool = AtomicBool::new(false);

/// Update the charging state shown by the battery icon. Called by the central
/// half on `ConnectionStatusChangeEvent`.
#[allow(dead_code)] // unused in the peripheral binary
pub fn set_charging(charging: bool) {
    CHARGING.store(charging, Ordering::Relaxed);
}

/// Current charging state, as last set by [`set_charging`]. Used by the battery
/// processor to tag the CW2015 reading's [`ChargeState`](rmk::types::battery::ChargeState).
#[allow(dead_code)] // always false on the peripheral binary
pub fn is_charging() -> bool {
    CHARGING.load(Ordering::Relaxed)
}

/// Shared SSD1306 OLED renderer.
///
/// Layout (top → bottom):
/// - A layer icon (`LAYER_0..3`) for the current layer.
/// - A battery icon with the "n%" level below it.
/// - A connection status line: "BLE{profile+1}" when BLE is advertising or
///   connected, "USB" while charging, otherwise blank.
///
/// `layer`, `battery`, and `ble_status` come from [`RenderContext`], which the
/// auto-generated `DisplayProcessor` keeps current by subscribing to the
/// relevant events.
#[derive(Default)]
pub struct DOIORenderer;

impl DisplayRenderer<BinaryColor> for DOIORenderer {
    fn render<D: DrawTarget<Color = BinaryColor>>(&mut self, ctx: &RenderContext, display: &mut D) {
        display.clear(BinaryColor::Off).ok();

        let bbox = display.bounding_box();
        let w = bbox.size.width as i32;
        let h = bbox.size.height as i32;

        // Reserve two stacked bands at the bottom: the battery (icon + level)
        // above the connection status line. The layer icon fills the region
        // above them.
        const CONN_H: i32 = 12; // connection status text ("USB" / "BLE1" / "BLE2" / "BLE3" / "CON")
        const BATT_H: i32 = 24; // battery icon + "n%" level
        let top_area_h = (h - CONN_H - BATT_H).max(2);

        let centered = TextStyleBuilder::new()
            .alignment(Alignment::Center)
            .baseline(Baseline::Middle)
            .build();

        // --- Layer icon centered in the top area ---
        let layer_icon: &[u8; 44] = match ctx.layer {
            3 => &icons::LAYER_3,
            2 => &icons::LAYER_2,
            1 => &icons::LAYER_1,
            _ => &icons::LAYER_0,
        };
        let layer_raw: ImageRaw<BinaryColor> = ImageRaw::new(layer_icon, LAYER_W as u32);
        Image::new(
            &layer_raw,
            Point::new((w - LAYER_W) / 2, (top_area_h - LAYER_H) / 2),
        )
        .draw(display)
        .ok();

        // --- Battery icon + level in the middle band ---
        // The icon sits near the top of the band; draw_battery_status renders
        // the "n%" level just below it.
        let batt_x = (w - BATTERY_W) / 2;
        let batt_y = top_area_h + 1;
        draw_battery_status(ctx, display, Point::new(batt_x, batt_y));

        // --- Connection status centered in the bottom band ---
        // Peripheral: the only meaningful status is the split link to the
        // central (`central_connected` => "CON"; it is only ever true on a
        // peripheral). Central: "USB" while charging, else the BLE profile
        // (Advertising/Connected => BLE mode; Inactive => USB/sleep).
        let mut conn_label: heapless::String<8> = heapless::String::new();
        if ctx.central_connected {
            let _ = write!(conn_label, "CON");
        } else if CHARGING.load(Ordering::Relaxed) {
            let _ = write!(conn_label, "USB");
        } else if ctx.ble_status.state == BleState::Advertising || ctx.ble_status.state == BleState::Connected {
            let _ = write!(conn_label, "BLE{}", ctx.ble_status.profile as u16 + 1);
        }
        Text::with_text_style(
            &conn_label,
            Point::new(w / 2, h - CONN_H / 2),
            MonoTextStyle::new(&FONT_6X10, BinaryColor::On),
            centered,
        )
        .draw(display)
        .ok();
    }
}

/// Assembled battery icon dimensions (see [`draw_battery_status`]): the
/// fragments are `BATTERY_ICON_SIZE` (16) wide, and the assembly is 9 rows tall.
const BATTERY_W: i32 = icons::BATTERY_ICON_SIZE as i32;
const BATTERY_H: i32 = 9;

/// Layer icon dimensions (`LAYER_0..3` in icons.rs): 16x22.
const LAYER_W: i32 = 16;
const LAYER_H: i32 = 22;

/// Draw the battery status at `top_left`: a 16x9 icon assembled from the
/// fragments in [`icons`], with the "n%" charge level centered just below it.
///
/// Icon assembly follows the scheme documented in icons.rs:
///   1. `BATTERY_TOP` (2 rows) — top border + walls.
///   2. `MID r0` transition row from the level fragment (`BATTERY_0..4`).
///   3. The center rows: the level's middle row (repeated) normally, or the
///      `BATTERY_CHARGING` bolt when [`CHARGING`] is set (USB connected).
///   4. The bottom mirrors the top for a symmetric cell (terminal on the right).
///
/// Nothing is drawn when the battery status is unavailable. When the level is
/// available but unknown, the icon shows full and the "n%" text is omitted.
fn draw_battery_status<D: DrawTarget<Color = BinaryColor>>(
    ctx: &RenderContext,
    display: &mut D,
    top_left: Point,
) {
    // Charge percentage; skip drawing entirely when no battery is reported.
    let pct: Option<u8> = match *ctx.battery {
        BatteryStatus::Available { level: Some(pct), .. } => Some(pct),
        // Available but level unknown (e.g. charging/full): show a full icon
        // but omit the number.
        BatteryStatus::Available { level: None, .. } => None,
        BatteryStatus::Unavailable => return,
    };
    let level = pct.map(|p| p as i32).unwrap_or(100);

    // Select the middle fragment from the charge level (see icons.rs comment).
    let mid: &[u8; 8] = if CHARGING.load(Ordering::Relaxed) {
        &icons::BATTERY_CHARGING
    } else if level <= 10 {
        &icons::BATTERY_0
    } else if level <= 25 {
        &icons::BATTERY_1
    } else if level <= 50 {
        &icons::BATTERY_2
    } else if level <= 75 {
        &icons::BATTERY_3
    } else {
        &icons::BATTERY_4
    };
    let top = &icons::BATTERY_TOP;

    #[rustfmt::skip]
    let bitmap: [u8; 18] = [
        top[0], top[1], top[2], top[3], // row 0+1: TOP r0+1 (top border + walls)
        mid[0], mid[1],                 // row 2: center
        mid[2], mid[3],                 // row 3: center
        mid[4], mid[5],                 // row 4: center
        mid[6], mid[7],                 // row 5: center
        mid[0], mid[1],                 // row 6: repeat of row 2
        top[2], top[3], top[0], top[1], // row 7+8: TOP r1+r0 (mirror)
    ];

    let raw: ImageRaw<BinaryColor> = ImageRaw::new(&bitmap, icons::BATTERY_ICON_SIZE);
    Image::new(&raw, top_left).draw(display).ok();

    // --- "n%" charge level centered just below the icon ---
    if let Some(pct) = pct {
        const LABEL_GAP: i32 = 2; // gap between icon bottom and text
        let mut label: heapless::String<5> = heapless::String::new();
        let _ = write!(label, "{}%", pct);
        let centered = TextStyleBuilder::new()
            .alignment(Alignment::Center)
            .baseline(Baseline::Middle)
            .build();
        Text::with_text_style(
            &label,
            // FONT_6X10 is 10 tall; +5 vertically centers it below icon + gap.
            Point::new(top_left.x + BATTERY_W / 2, top_left.y + BATTERY_H + LABEL_GAP + 5),
            MonoTextStyle::new(&FONT_6X10, BinaryColor::On),
            centered,
        )
        .draw(display)
        .ok();
    }
}
