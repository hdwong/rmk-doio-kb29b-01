//! OLED + battery processor driving a shared I2C bus.
//!
//! # Why this exists
//!
//! The CW2015 fuel gauge and the SSD1306 OLED sit on the *same* physical I2C bus
//! (SDA=P0_06, SCL=P0_08, `TWISPI0`). RMK's generated display support builds its
//! own `Twim` for that bus and hands it exclusively to the SSD1306 driver, with
//! no hook to share it. So instead of using the built-in `[split.*.display]`
//! config, we own the bus here: one [`Twim`] is wrapped in a [`Mutex`] and split
//! into two [`I2cDevice`] handles — one for the OLED, one for the CW2015.
//!
//! [`OledBatteryProcessor`] then does what RMK's `DisplayProcessor` did (render
//! keyboard state via the shared [`DOIORenderer`]) *plus* reads the CW2015 every
//! 3 s and feeds the precise state-of-charge into the render context.
//!
//! A [`NoopRawMutex`] is sufficient for the bus: only this one processor task
//! touches it (both the OLED flush and the CW2015 read happen inside its own
//! event/poll loop, never concurrently).

use embassy_embedded_hal::shared_bus::asynch::i2c::I2cDevice;
use embassy_nrf::twim::Twim;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, with_timeout};
use rmk::display::ssd1306::mode::BufferedGraphicsModeAsync;
use rmk::display::ssd1306::prelude::{DisplayRotation, DisplaySize128x32, I2CInterface};
use rmk::display::ssd1306::{I2CDisplayInterface, Ssd1306Async};
use rmk::display::{DisplayDriver, DisplayRenderer, RenderContext};
use rmk::event::{BatteryStatusEvent, CentralConnectedEvent, ConnectionStatusChangeEvent, LayerChangeEvent};
use rmk::macros::processor;
use rmk::types::battery::{BatteryStatus, ChargeState};
use static_cell::StaticCell;

use crate::cw2015::Cw2015;
use crate::display::renderer::{DOIORenderer, is_charging};

// TWIM interrupt binding for the shared bus. RMK's generated `Irqs` doesn't bind
// TWISPI0 (there's no `[split.*.display]` config), so we bind it ourselves. This
// is a distinct struct from RMK's `Irqs`; each interrupt is still bound exactly
// once across the binary.
embassy_nrf::bind_interrupts!(pub struct TwimIrqs {
    TWISPI0 => embassy_nrf::twim::InterruptHandler<embassy_nrf::peripherals::TWISPI0>;
});

/// I2C address of the SSD1306 panel (from the original `keyboard.toml` config).
const OLED_ADDRESS: u8 = 0x3C;

/// The shared async I2C bus over `TWISPI0`.
pub type OledI2cBus = Mutex<NoopRawMutex, Twim<'static>>;
/// A cloneable handle to the shared bus for one device on it.
pub type OledI2cDevice = I2cDevice<'static, NoopRawMutex, Twim<'static>>;
/// The buffered-graphics SSD1306 built on top of the shared bus.
pub type OledDisplay =
    Ssd1306Async<I2CInterface<OledI2cDevice>, DisplaySize128x32, BufferedGraphicsModeAsync<DisplaySize128x32>>;

/// Wrap the raw `Twim` in a `'static` shared bus and split it into the SSD1306
/// display and the CW2015 fuel gauge, both talking to the same wires.
///
/// Called exactly once (per binary) from the processor's registration function,
/// which is the only place the raw `Twim` and interrupt binding are available.
pub fn split_bus(twim: Twim<'static>) -> (OledDisplay, Cw2015<OledI2cDevice>) {
    static BUS: StaticCell<OledI2cBus> = StaticCell::new();
    let bus = BUS.init(Mutex::new(twim));

    let display_iface = I2CDisplayInterface::new_custom_address(I2cDevice::new(bus), OLED_ADDRESS);
    let display = Ssd1306Async::new(display_iface, DisplaySize128x32, DisplayRotation::Rotate90)
        .into_buffered_graphics_mode();

    let cw2015 = Cw2015::new(I2cDevice::new(bus));
    (display, cw2015)
}

/// Renders keyboard state on the OLED and refreshes the battery level from the
/// CW2015 every 3 seconds.
///
/// Subscribes to the same events the built-in `DisplayProcessor` used for the
/// bits [`DOIORenderer`] actually draws: the active layer, the BLE status, and
/// the split-link (central-connected) state. The battery level comes from the
/// CW2015 poll rather than from `BatteryStatusEvent`.
#[processor(
    subscribe = [LayerChangeEvent, ConnectionStatusChangeEvent, CentralConnectedEvent],
    poll_interval = 3000
)]
pub struct OledBatteryProcessor {
    display: OledDisplay,
    cw2015: Cw2015<OledI2cDevice>,
    renderer: DOIORenderer,
    ctx: RenderContext,
    /// Whether the SSD1306 init sequence has run (done lazily on first render).
    display_ready: bool,
    /// Whether the CW2015 has been woken (done lazily on first poll).
    cw_ready: bool,
}

/// Cap for any single bus transaction. Nothing here should take longer than a
/// framebuffer flush (~50 ms at 100 kHz); the cap only exists so a missing or
/// misbehaving device on the shared bus can never wedge this task.
const BUS_TIMEOUT: Duration = Duration::from_millis(250);

impl OledBatteryProcessor {
    /// Construct the processor. Deliberately does **no** bus I/O: registration
    /// functions run inline during boot, before the keyboard/BLE tasks start, so
    /// blocking here on a shared-bus hiccup would stall the whole firmware. All
    /// I2C work (panel init, fuel-gauge init, reads) is deferred to the run loop.
    pub fn new(display: OledDisplay, cw2015: Cw2015<OledI2cDevice>) -> Self {
        Self {
            display,
            cw2015,
            renderer: DOIORenderer::default(),
            ctx: RenderContext::default(),
            display_ready: false,
            cw_ready: false,
        }
    }

    /// Redraw the current render context onto the panel, running the SSD1306
    /// init sequence on first use. Bus ops are timeout-guarded.
    async fn render(&mut self) {
        if !self.display_ready {
            // Use the `DisplayDriver` trait methods explicitly: SSD1306's
            // inherent `init`/`flush` shadow them and return a `Result`.
            if with_timeout(BUS_TIMEOUT, DisplayDriver::init(&mut self.display)).await.is_err() {
                defmt::warn!("OLED init timed out");
                return;
            }
            self.display_ready = true;
        }
        self.renderer.render(&self.ctx, &mut self.display);
        if with_timeout(BUS_TIMEOUT, DisplayDriver::flush(&mut self.display)).await.is_err() {
            defmt::warn!("OLED flush timed out");
        }
    }

    /// Called every 3 s: wake the CW2015 on first use, refresh the battery
    /// level, and redraw.
    async fn poll(&mut self) {
        if !self.cw_ready {
            match with_timeout(BUS_TIMEOUT, self.cw2015.init()).await {
                Ok(Ok(())) => self.cw_ready = true,
                _ => defmt::warn!("CW2015 init failed/timed out"),
            }
        }
        self.ctx.battery = read_battery(&mut self.cw2015).await;
        self.render().await;
    }

    async fn on_layer_change_event(&mut self, event: LayerChangeEvent) {
        self.ctx.layer = event.0;
        self.render().await;
    }

    async fn on_connection_status_change_event(&mut self, event: ConnectionStatusChangeEvent) {
        self.ctx.ble_status = event.0.ble;
        self.render().await;
    }

    async fn on_central_connected_event(&mut self, event: CentralConnectedEvent) {
        self.ctx.central_connected = event.connected;
        self.render().await;
    }
}

/// Read the CW2015 state-of-charge and wrap it as a [`BatteryStatusEvent`] for
/// the render context. On an I2C error the battery is reported as unavailable
/// (the renderer then omits the level) and a warning is logged.
async fn read_battery(cw2015: &mut Cw2015<OledI2cDevice>) -> BatteryStatusEvent {
    match with_timeout(BUS_TIMEOUT, cw2015.read_soc()).await {
        Ok(Ok(level)) => {
            defmt::info!("CW2015 SOC: {}%", level);
            let charge_state = if is_charging() {
                ChargeState::Charging
            } else {
                ChargeState::Discharging
            };
            BatteryStatusEvent(BatteryStatus::Available {
                charge_state,
                level: Some(level),
            })
        }
        _ => {
            defmt::warn!("CW2015 SOC read failed/timed out");
            BatteryStatusEvent(BatteryStatus::Unavailable)
        }
    }
}
