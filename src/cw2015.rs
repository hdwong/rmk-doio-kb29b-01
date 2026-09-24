//! Driver for the CW2015 Li-ion fuel-gauge IC (CellWise).
//!
//! The CW2015 is a sensing-resistor-free "gas gauge" that tracks the battery's
//! state-of-charge (SOC) with a 14-bit sigma-delta ADC and CellWise's FastCali
//! algorithm. It replaces the imprecise raw-ADC voltage reading with a proper
//! SOC estimate (datasheet: `docs/CW2015CHBD.pdf`).
//!
//! It is wired onto the same I2C bus as the OLED (SDA=P0_06, SCL=P0_08). The bus
//! is shared through an [`embassy_embedded_hal`] `I2cDevice`, so this driver is
//! generic over any [`embedded_hal_async::i2c::I2c`] implementation.
//!
//! Register map (see datasheet §"Register Map"):
//! - `0x00` VERSION   (R)
//! - `0x02` VCELL     (R)  14-bit cell voltage, 305 µV / LSB
//! - `0x04` SOC       (R)  high byte = integer %, low byte = 1/256 %
//! - `0x08` CONFIG    (R/W) alert threshold
//! - `0x0A` MODE      (R/W) sleep / quick-start / POR control

use embedded_hal_async::i2c::I2c;

/// 7-bit I2C address of the CW2015 (fixed at `0b110_0010`). Combined with the
/// R/W bit this is the datasheet's write command `0xC4` / read command `0xC5`.
const CW2015_ADDR: u8 = 0x62;

// Register addresses.
const REG_VCELL: u8 = 0x02;
const REG_SOC: u8 = 0x04;
const REG_MODE: u8 = 0x0A;

/// MODE value that clears the sleep bits and runs the gauge normally.
/// The power-on default is `0xC0` (sleep bits = `11`), so the IC must be woken.
const MODE_WAKE: u8 = 0x00;
/// MODE value that requests a quick-start (QSTRT bits = `11`). Quick-start
/// re-seeds the SOC estimate the way a fresh power-up would, reducing the error
/// of the first reading.
const MODE_QUICK_START: u8 = 0x30;

/// CW2015 fuel gauge over a shared async I2C bus.
pub struct Cw2015<I2C> {
    i2c: I2C,
}

impl<I2C: I2c> Cw2015<I2C> {
    /// Wrap an I2C bus handle. Call [`init`](Self::init) before reading.
    pub fn new(i2c: I2C) -> Self {
        Self { i2c }
    }

    /// Wake the IC from sleep and issue a quick-start so the first SOC reading
    /// is a sensible estimate rather than the sleep-mode default.
    pub async fn init(&mut self) -> Result<(), I2C::Error> {
        // Clear the sleep bits (default MODE is 0xC0 = asleep).
        self.i2c.write(CW2015_ADDR, &[REG_MODE, MODE_WAKE]).await?;
        // Quick-start, then return to normal mode.
        self.i2c
            .write(CW2015_ADDR, &[REG_MODE, MODE_QUICK_START])
            .await?;
        embassy_time::Timer::after_millis(2).await;
        self.i2c.write(CW2015_ADDR, &[REG_MODE, MODE_WAKE]).await?;
        Ok(())
    }

    /// Read the state-of-charge in whole percent (0..=100).
    ///
    /// The SOC register's high byte already holds the integer percentage; the
    /// low byte is the 1/256 % fraction, which we drop. The value is clamped to
    /// 100 because the algorithm can transiently report slightly above full.
    pub async fn read_soc(&mut self) -> Result<u8, I2C::Error> {
        let mut buf = [0u8; 2];
        self.i2c
            .write_read(CW2015_ADDR, &[REG_SOC], &mut buf)
            .await?;
        Ok(buf[0].min(100))
    }

    /// Read the cell voltage in millivolts.
    ///
    /// VCELL is a 14-bit value with a resolution of 305 µV / LSB. Only the low
    /// 6 bits of the high byte are significant (the top 2 bits read as 0).
    #[allow(dead_code)] // exposed for diagnostics; not required for the % display
    pub async fn read_millivolts(&mut self) -> Result<u16, I2C::Error> {
        let mut buf = [0u8; 2];
        self.i2c
            .write_read(CW2015_ADDR, &[REG_VCELL], &mut buf)
            .await?;
        let raw = (((buf[0] as u32) & 0x3F) << 8) | buf[1] as u32;
        // raw * 305 µV = raw * 305 / 1000 mV.
        Ok((raw * 305 / 1000) as u16)
    }
}
