//! `esptool` command construction and domain types.
//!
//! `SPEC.md` §9: esptool operations (chip/flash info, erase, write, verify,
//! reset) are presented separately from the `mpremote` filesystem browser and
//! own their own tool. Mirrors [`super::commands`]/[`super::parse`]'s split:
//! [`commands`] builds invocations, [`parse`] reads their output.

pub mod commands;
pub mod parse;

/// ESP chip families with a flash offset commonly used in practice.
///
/// Not exhaustive --- esptool (`--chip`) supports more. An unrecognised or
/// newer chip is still flashable: leave the family unset and edit the offset
/// and any extra flags by hand (`SPEC.md` §8's "never guess, always allow a
/// manual override" applies here just as it does to device selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipFamily {
    Esp8266,
    Esp32,
    Esp32S2,
    Esp32S3,
    Esp32C3,
    Esp32C6,
}

impl ChipFamily {
    pub const ALL: &'static [ChipFamily] = &[
        Self::Esp8266,
        Self::Esp32,
        Self::Esp32S2,
        Self::Esp32S3,
        Self::Esp32C3,
        Self::Esp32C6,
    ];

    /// The value passed to esptool's `--chip`.
    pub const fn esptool_id(self) -> &'static str {
        match self {
            Self::Esp8266 => "esp8266",
            Self::Esp32 => "esp32",
            Self::Esp32S2 => "esp32s2",
            Self::Esp32S3 => "esp32s3",
            Self::Esp32C3 => "esp32c3",
            Self::Esp32C6 => "esp32c6",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Esp8266 => "ESP8266",
            Self::Esp32 => "ESP32",
            Self::Esp32S2 => "ESP32-S2",
            Self::Esp32S3 => "ESP32-S3",
            Self::Esp32C3 => "ESP32-C3",
            Self::Esp32C6 => "ESP32-C6",
        }
    }

    /// The `mcu` filter value micropython.org/download/ expects for this
    /// family --- identical to [`ChipFamily::esptool_id`] today, kept as a
    /// separate method because the two ids are different concerns (one
    /// esptool's, one the download site's) that happen to coincide.
    pub const fn micropython_mcu_filter(self) -> &'static str {
        self.esptool_id()
    }

    /// Where a MicroPython combined firmware image is conventionally written.
    ///
    /// ESP32 classic keeps its bootloader at `0x1000`; every later family
    /// esptool supports here moves it to `0x0`. This only pre-fills a
    /// starting point in the flash options screen --- always editable, and
    /// never overwrites a value the user already touched.
    pub const fn default_offset(self) -> &'static str {
        match self {
            Self::Esp32 => "0x1000",
            _ => "0x0",
        }
    }
}

/// SPI flash mode, matching esptool's `--flash-mode` choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashMode {
    Keep,
    Qio,
    Qout,
    Dio,
    Dout,
}

impl FlashMode {
    pub const ALL: &'static [FlashMode] =
        &[Self::Keep, Self::Qio, Self::Qout, Self::Dio, Self::Dout];

    pub const fn esptool_id(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Qio => "qio",
            Self::Qout => "qout",
            Self::Dio => "dio",
            Self::Dout => "dout",
        }
    }
}

/// SPI flash frequency, matching esptool's `--flash-freq` choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashFreq {
    Keep,
    M80,
    M60,
    M40,
    M26,
    M20,
}

impl FlashFreq {
    pub const ALL: &'static [FlashFreq] = &[
        Self::Keep,
        Self::M80,
        Self::M60,
        Self::M40,
        Self::M26,
        Self::M20,
    ];

    pub const fn esptool_id(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::M80 => "80m",
            Self::M60 => "60m",
            Self::M40 => "40m",
            Self::M26 => "26m",
            Self::M20 => "20m",
        }
    }
}

/// SPI flash size, matching esptool's `--flash-size` choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashSize {
    Detect,
    Keep,
    Mb1,
    Mb2,
    Mb4,
    Mb8,
    Mb16,
}

impl FlashSize {
    pub const ALL: &'static [FlashSize] = &[
        Self::Detect,
        Self::Keep,
        Self::Mb1,
        Self::Mb2,
        Self::Mb4,
        Self::Mb8,
        Self::Mb16,
    ];

    pub const fn esptool_id(self) -> &'static str {
        match self {
            Self::Detect => "detect",
            Self::Keep => "keep",
            Self::Mb1 => "1MB",
            Self::Mb2 => "2MB",
            Self::Mb4 => "4MB",
            Self::Mb8 => "8MB",
            Self::Mb16 => "16MB",
        }
    }
}

/// Everything about the connected chip that [`parse::parse_device_details`]
/// could read off an esptool banner --- identity and flash geometry, for the
/// Dashboard's device panel. Every field is independently optional because no
/// single esptool sub-command prints all of them (`chip-id` never mentions
/// flash; `flash-id` does not repeat `Chip ID:`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceDetails {
    pub family: Option<ChipFamily>,
    pub revision: Option<String>,
    pub features: Option<String>,
    pub crystal_mhz: Option<String>,
    pub mac: Option<String>,
    pub flash_manufacturer: Option<String>,
    pub flash_device: Option<String>,
    pub flash_size: Option<String>,
}

impl DeviceDetails {
    /// Layers `other`'s known fields over `self`, so a command that does not
    /// mention a field (e.g. `chip-id` never prints flash geometry) cannot
    /// erase what an earlier run already learned. A manual chip override is
    /// tracked separately by [`crate::flash::ChipGuess`] and is not this
    /// struct's concern.
    pub fn merge(&mut self, other: DeviceDetails) {
        if other.family.is_some() {
            self.family = other.family;
        }
        if other.revision.is_some() {
            self.revision = other.revision;
        }
        if other.features.is_some() {
            self.features = other.features;
        }
        if other.crystal_mhz.is_some() {
            self.crystal_mhz = other.crystal_mhz;
        }
        if other.mac.is_some() {
            self.mac = other.mac;
        }
        if other.flash_manufacturer.is_some() {
            self.flash_manufacturer = other.flash_manufacturer;
        }
        if other.flash_device.is_some() {
            self.flash_device = other.flash_device;
        }
        if other.flash_size.is_some() {
            self.flash_size = other.flash_size;
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == DeviceDetails::default()
    }
}

/// User-editable flags for `write-flash`/`verify-flash`.
///
/// `extra_args` is free text, whitespace-tokenized and appended verbatim ---
/// the escape hatch for anything not covered by the structured presets
/// (`commands::write_flash` skips a preset flag entirely when the same flag
/// name already appears there, so a custom value always wins and no flag is
/// ever duplicated).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlashOptions {
    pub flash_mode: Option<FlashMode>,
    pub flash_freq: Option<FlashFreq>,
    pub flash_size: Option<FlashSize>,
    pub extra_args: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcu_filter_matches_the_download_sites_own_filter_values() {
        // Verified against the live `?mcu=` filter list on
        // micropython.org/download/ during implementation.
        assert_eq!(ChipFamily::Esp8266.micropython_mcu_filter(), "esp8266");
        assert_eq!(ChipFamily::Esp32.micropython_mcu_filter(), "esp32");
        assert_eq!(ChipFamily::Esp32S2.micropython_mcu_filter(), "esp32s2");
        assert_eq!(ChipFamily::Esp32S3.micropython_mcu_filter(), "esp32s3");
        assert_eq!(ChipFamily::Esp32C3.micropython_mcu_filter(), "esp32c3");
        assert_eq!(ChipFamily::Esp32C6.micropython_mcu_filter(), "esp32c6");
    }
}
