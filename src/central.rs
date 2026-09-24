#![no_main]
#![no_std]

mod cw2015;
mod display;

use defmt::info;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use rmk::macros::rmk_central;
use rmk::macros::processor;
use rmk::event::ConnectionStatusChangeEvent;
use rmk::types::ble::BleState;
use rmk::types::connection::UsbState;

use crate::display::renderer::set_charging;

#[processor(
    subscribe = [ConnectionStatusChangeEvent],
    poll_interval = 500
)]

pub struct CentralProcessor {
    led4: Output<'static>,  // BLE LED
    _led3: Output<'static>,  // Low battery indicator (light if battery level is lower than 20%)
    // OLED VCC enable (P0.17, high = powered). Held here so the pin stays
    // driven high for the program's lifetime; dropping it would release the pin.
    _oled_vcc: Output<'static>,
    ble_state: BleState,   // BLE status
    // Blink phase 0..3: 0 = on (1 tick), 1..3 = off (3 ticks). Ticks are 500ms.
    ble_blink_phase: u8,
}

impl CentralProcessor {
    async fn on_connection_status_change_event(&mut self, event: ConnectionStatusChangeEvent) {
        // USB plugged in => charging (surfaced on the OLED via the shared renderer).
        let usb_connected = matches!(event.0.usb, UsbState::Configured | UsbState::Suspended);
        set_charging(usb_connected);

        if usb_connected {
            // When the keyboard is plugged in USB mode, the BLE state is inactive.
            self.ble_state = BleState::Inactive;
            return;
        }
        let state = event.0.ble.state;
        self.ble_state = state;
        match state {
            BleState::Advertising => info!("BLE: Advertising"),
            BleState::Connected => info!("BLE: Connected"),
            _ => info!("BLE: Inactive"),
        }
    }

    // Called every 500ms for BLE advertising blink
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
                // Turn off BLE LED
                self.led4.set_low();
            }
        }
    }
}

#[rmk_central]
mod keyboard_central {
    // Initialize Central LEDs:
    // LED4 (P0.13): BLE status indicator (Blue)
    // LED3 (P0.15): Low battery indicator (Red)
    #[register_processor(poll)]
    fn central() -> CentralProcessor {
        use crate::CentralProcessor;

        // LED4 - P0.13: BLE status indicator, initial Low (Off, high-active)
        let led4 = Output::new(
            p.P0_13,
            Level::Low,
            OutputDrive::Standard,
        );

        // LED3 - P0.15: Low battery indicator, initial Low (Off, high-active)
        let led3 = Output::new(
            p.P0_15,
            Level::Low,
            OutputDrive::Standard,
        );

        // OLED VCC - P0.17: start Low, wait 500ms for the rail to settle, then
        // drive High to power the SSD1306. This body is inlined into the async
        // init (before `#display_init`), so `.await` is valid here and the panel
        // is powered before the first render.
        let mut oled_vcc = Output::new(
            p.P0_17,
            Level::Low,
            OutputDrive::Standard,
        );
        embassy_time::Timer::after_millis(500).await;
        oled_vcc.set_high();

        CentralProcessor {
            led4,
            _led3: led3,
            _oled_vcc: oled_vcc,
            ble_state: BleState::Inactive,
            ble_blink_phase: 0,
        }
    }

    // OLED + CW2015 fuel gauge on the shared TWISPI0 bus (SDA=P0_06, SCL=P0_08).
    // Registered after `central()` so the OLED VCC rail is already powered when
    // this initializes the panel. See src/display/oled.rs.
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
