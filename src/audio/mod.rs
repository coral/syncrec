//! Device discovery and stream configuration.

pub mod capture;
pub mod meters;
pub mod session;
pub mod writer;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, DeviceId, SampleFormat, StreamConfig, SupportedStreamConfig};

/// What we want to end up with, always. If the device cannot give us this rate we
/// take what it offers and fold the conversion into the same resample pass that
/// corrects drift, so there is only ever one resample.
pub const TARGET_RATE: u32 = 48_000;

/// A device as shown in the picker.
///
/// Carries cpal's `DeviceId` rather than just the name: two interfaces can report
/// the same name, and the id round-trips through `Display`/`FromStr` so a chosen
/// device can be remembered across runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceChoice {
    pub id: DeviceId,
    pub name: String,
    pub is_default: bool,
}

impl std::fmt::Display for DeviceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_default {
            write!(f, "{} (default)", self.name)
        } else {
            f.write_str(&self.name)
        }
    }
}

/// List the input devices on the default host.
pub fn input_devices() -> Result<Vec<DeviceChoice>> {
    let host = cpal::default_host();
    let default_id = host.default_input_device().and_then(|d| d.id().ok());

    let mut out = Vec::new();
    for device in host.input_devices().context("enumerating input devices")? {
        // Anything without an id cannot be reopened reliably, and anything without
        // input configs would fail at stream build time.
        let (Ok(id), Ok(desc)) = (device.id(), device.description()) else {
            continue;
        };
        if !device.supports_input() {
            continue;
        }
        let is_default = Some(&id) == default_id.as_ref();
        out.push(DeviceChoice {
            id,
            name: desc.name().to_string(),
            is_default,
        });
    }

    // Default first, then alphabetical, so the list does not reshuffle on refresh.
    out.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Re-open a previously chosen device.
pub fn find_device(choice: &DeviceChoice) -> Result<Device> {
    cpal::default_host()
        .device_by_id(&choice.id)
        .ok_or_else(|| anyhow!("input device '{}' is no longer present", choice.name))
}

/// The stream configuration we settled on, and what we had to give up to get it.
#[derive(Debug, Clone)]
pub struct Negotiated {
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
    pub rate: u32,
    pub channels: u16,
    /// True when the device agreed to run at 48 kHz, so no rate conversion is needed
    /// beyond the drift correction.
    pub native_target_rate: bool,
}

impl Negotiated {
    /// Frames per second as f64, for time math.
    pub fn rate_f64(&self) -> f64 {
        self.rate as f64
    }
}

/// Ask the device for 48 kHz; accept its default rate if it refuses.
///
/// Channel count and sample format are taken from the device's own default, which is
/// the configuration it is happiest in. We only try to move the rate.
pub fn negotiate(device: &Device) -> Result<Negotiated> {
    let default: SupportedStreamConfig = device
        .default_input_config()
        .context("querying default input config")?;

    let channels = default.channels();
    let format = default.sample_format();

    if !supported_capture_format(format) {
        return Err(anyhow!(
            "device reports sample format {format:?}, which syncrec cannot capture"
        ));
    }

    // Look for a supported range with the same shape that spans 48 kHz.
    let at_target = device
        .supported_input_configs()
        .context("querying supported input configs")?
        .filter(|r| r.channels() == channels && r.sample_format() == format)
        .find_map(|r| r.try_with_sample_rate(TARGET_RATE));

    let chosen = at_target.unwrap_or(default);
    let rate = chosen.sample_rate();

    Ok(Negotiated {
        config: chosen.config(),
        sample_format: format,
        rate,
        channels,
        native_target_rate: rate == TARGET_RATE,
    })
}

/// Formats `capture::build_stream` knows how to read.
pub fn supported_capture_format(f: SampleFormat) -> bool {
    matches!(
        f,
        SampleFormat::F32
            | SampleFormat::I16
            | SampleFormat::I32
            | SampleFormat::I8
            | SampleFormat::U8
            | SampleFormat::U16
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(name: &str, is_default: bool) -> DeviceChoice {
        let host = *cpal::available_hosts()
            .first()
            .expect("every platform has at least one host");
        DeviceChoice {
            id: DeviceId::new(host, name),
            name: name.into(),
            is_default,
        }
    }

    #[test]
    fn device_choice_marks_the_default() {
        assert_eq!(
            choice("Scarlett 2i2", true).to_string(),
            "Scarlett 2i2 (default)"
        );
        assert_eq!(choice("Scarlett 2i2", false).to_string(), "Scarlett 2i2");
    }

    #[test]
    fn devices_sort_default_first_then_alphabetically() {
        let mut v = [
            choice("Zoom", false),
            choice("Apogee", false),
            choice("Built-in", true),
        ];
        v.sort_by(|a, b| {
            b.is_default
                .cmp(&a.is_default)
                .then_with(|| a.name.cmp(&b.name))
        });
        let names: Vec<_> = v.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Built-in", "Apogee", "Zoom"]);
    }

    #[test]
    fn float_and_common_int_formats_are_capturable() {
        assert!(supported_capture_format(SampleFormat::F32));
        assert!(supported_capture_format(SampleFormat::I16));
        assert!(supported_capture_format(SampleFormat::I32));
        assert!(!supported_capture_format(SampleFormat::F64));
    }
}
