#![no_main]
#![no_std]

use defmt::info;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use rmk::macros::rmk_central;
use rmk::macros::processor;
use rmk::event::{
    ConnectionStatusChangeEvent
};
use rmk::types::ble::BleState;
use rmk::types::connection::UsbState;

#[processor(
  subscribe = [ConnectionStatusChangeEvent],
  poll_interval = 500
)]

pub struct CentralProcessor {
  led4: Output<'static>,  // BLE LED
  ble_state: BleState,   // BLE status
  // Blink phase 0..3: 0 = on (1 tick), 1..3 = off (3 ticks). Ticks are 500ms.
  ble_blink_phase: u8,
}

impl CentralProcessor {
  async fn on_connection_status_change_event(&mut self, event: ConnectionStatusChangeEvent) {
      if event.0.usb == UsbState::Configured || event.0.usb == UsbState::Suspended {
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
    // LED4 (P0.29): BLE status indicator (Blue)
    // TODO: LED3 (P0.31): Low battery indicator (Red)
    #[register_processor(poll)]
    fn central() -> CentralProcessor {
        use crate::CentralProcessor;

        info!("Initializing Central LEDs: P0.29");

        // LED4 - P0.29: BLE status indicator, initial Low (Off, high-active)
        let led4 = Output::new(
            p.P0_29,
            Level::Low,
            OutputDrive::Standard,
        );

        CentralProcessor {
            led4,
            ble_state: BleState::Inactive,
            ble_blink_phase: 0,
        }
    }
}
