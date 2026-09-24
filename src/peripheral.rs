#![no_main]
#![no_std]

mod cw2015;
mod display;

use defmt::info;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use rmk::event::CentralConnectedEvent;
use rmk::macros::processor;
use rmk::macros::rmk_peripheral;
use rmk::types::ble::BleState;

/// Drives the peripheral's split-link status LED (mirrors `CentralProcessor` in
/// src/central.rs).
///
/// The peripheral has no host BLE connection of its own; the LED instead
/// reflects whether the split link to the central is up. We reuse [`BleState`]
/// so the blink logic in [`poll`](Self::poll) stays identical to the central's:
/// - Not connected to central => `Advertising` => LED blinks (searching).
/// - Connected to central     => `Connected`   => LED off.
///
/// The state is updated from [`CentralConnectedEvent`], which RMK's split
/// peripheral task publishes whenever the central link comes up or goes down.
#[processor(
    subscribe = [CentralConnectedEvent],
    poll_interval = 500
)]
pub struct PeripheralProcessor {
    led4: Output<'static>,   // Split-link status indicator (Blue)
    _led3: Output<'static>,  // Low battery indicator; held so the pin stays driven
    // OLED VCC enable (P0.22, high = powered). Held here so the pin stays driven
    // high for the program's lifetime; dropping it would release the pin.
    _oled_vcc: Output<'static>,
    ble_state: BleState,   // Central-link status, expressed via BleState
    // Blink phase 0..3: 0 = on (1 tick), 1..3 = off (3 ticks). Ticks are 500ms.
    ble_blink_phase: u8,
}

impl PeripheralProcessor {
    async fn on_central_connected_event(&mut self, event: CentralConnectedEvent) {
        // Map the split-link state onto BleState so `poll` mirrors the central:
        // connected => Connected (LED off), disconnected => Advertising (blink).
        if event.connected {
            self.ble_state = BleState::Connected;
            info!("Central: Connected");
        } else {
            self.ble_state = BleState::Advertising;
            info!("Central: Disconnected");
        }
    }

    // Called every 500ms to drive the "searching for central" blink.
    async fn poll(&mut self) {
        match self.ble_state {
            BleState::Advertising => {
                // Phase 0 = LED on, phases 1..3 = LED off (0.5s on, 1.5s off)
                let ble_led_on = self.ble_blink_phase == 0;
                if ble_led_on {
                    self.led4.set_high();
                } else {
                    self.led4.set_low();
                }
                self.ble_blink_phase = (self.ble_blink_phase + 1) % 4;
            }
            _ => {
                // Connected (or initial Inactive): LED off.
                self.led4.set_low();
            }
        }
    }
}

#[rmk_peripheral(id = 0)]
mod keyboard_peripheral {
    // Initialize Peripheral LEDs:
    // LED4 (P0.29): Split-link status indicator (Blue)
    // LED3 (P0.31): Low battery indicator (Red)
    #[register_processor(poll)]
    fn peripheral() -> PeripheralProcessor {
        use crate::PeripheralProcessor;

        // LED4 - P0.29: Split-link status indicator, initial Low (Off, high-active)
        let led4 = Output::new(
            p.P0_29,
            Level::Low,
            OutputDrive::Standard,
        );

        // LED3 - P0.31: Low battery indicator, initial Low (Off, high-active)
        let led3 = Output::new(
            p.P0_31,
            Level::Low,
            OutputDrive::Standard,
        );

        // OLED VCC - P0.22: start Low, wait 500ms for the rail to settle, then
        // drive High to power the SSD1306. This body is inlined into the async
        // init (before `#display_init`), so `.await` is valid here and the panel
        // is powered before the first render.
        let mut oled_vcc = Output::new(
            p.P0_22,
            Level::Low,
            OutputDrive::Standard,
        );
        embassy_time::Timer::after_millis(500).await;
        oled_vcc.set_high();

        PeripheralProcessor {
            led4,
            _led3: led3,
            _oled_vcc: oled_vcc,
            ble_state: BleState::Inactive,
            ble_blink_phase: 0,
        }
    }

    // OLED + CW2015 fuel gauge on the shared TWISPI0 bus (SDA=P0_06, SCL=P0_08).
    // Registered after `peripheral()` so the OLED VCC rail is already powered
    // when this initializes the panel. See src/display/oled.rs.
    #[register_processor(poll)]
    fn oled_battery() -> crate::display::oled::OledBatteryProcessor {
        use crate::display::oled;

        // TWIM EasyDMA scratch buffer (can't DMA directly from flash). Sized to
        // match RMK's generated display bus.
        static TX_BUF: ::static_cell::StaticCell<[u8; 256]> = ::static_cell::StaticCell::new();
        let tx_buf = TX_BUF.init([0u8; 256]);

        let twim = ::embassy_nrf::twim::Twim::new(
            p.TWISPI0,
            oled::TwimIrqs,
            p.P0_06,
            p.P0_08,
            ::embassy_nrf::twim::Config::default(),
            tx_buf,
        );
        let (display, cw2015) = oled::split_bus(twim);
        oled::OledBatteryProcessor::new(display, cw2015)
    }
}
