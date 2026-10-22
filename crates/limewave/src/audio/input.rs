//! Input devices and the stream that feeds a [`Feed`] (the device side of recording).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, I24, SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig, U24};

use super::record::{Feed, InputErrors};

/// Frames of the conversion buffer used for non-`f32` devices.
const SCRATCH_FRAMES: usize = 16_384;

/// An input device and its default input configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    pub name: String,
    pub channels: usize,
    pub rate: u32,
    pub is_default: bool,
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map_or_else(|_| "Unknown device".to_string(), |d| d.name().to_string())
}

/// The host's input devices with their default input configs, the system default first.
/// Devices whose config cannot be read are left out.
pub fn input_devices() -> Vec<InputDevice> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().map(|d| device_name(&d));
    let mut list: Vec<InputDevice> = host
        .input_devices()
        .into_iter()
        .flatten()
        .filter_map(|device| {
            let config = device.default_input_config().ok()?;
            let name = device_name(&device);
            Some(InputDevice {
                is_default: default_name.as_deref() == Some(name.as_str()),
                name,
                channels: usize::from(config.channels()),
                rate: config.sample_rate(),
            })
        })
        .collect();
    // Stable: the default moves to the front, the rest keep the host's order.
    list.sort_by_key(|d| !d.is_default);
    // Only one entry may claim to be the default, even if two devices share its name.
    for d in list.iter_mut().skip(1) {
        d.is_default = false;
    }
    list
}

/// How many input devices the host lists, without reading their configs: cheap enough to
/// ask again whenever Record might become usable.
pub fn input_device_count() -> usize {
    cpal::default_host()
        .input_devices()
        .map_or(0, Iterator::count)
}

/// An input device found by name, with the config it opens at: its default input config as
/// it is now, which may differ from when the devices were listed.
pub struct PreparedInput {
    name: String,
    device: cpal::Device,
    config: SupportedStreamConfig,
}

/// Finds the named input device and reads its current default input config.
pub fn prepare_input(name: &str) -> Result<PreparedInput, String> {
    let host = cpal::default_host();
    let device = host
        .input_devices()
        .map_err(|e| format!("Could not list input devices: {e}"))?
        .find(|d| device_name(d) == name)
        .ok_or_else(|| format!("Input device {name} was not found"))?;
    let config = device
        .default_input_config()
        .map_err(|e| format!("Could not open {name}: {e}"))?;
    if config.channels() == 0 {
        return Err(format!("Could not open {name}: it has no input channels"));
    }
    Ok(PreparedInput {
        name: name.to_string(),
        device,
        config,
    })
}

impl PreparedInput {
    /// The rate the stream opens at, which is the take's.
    pub fn rate(&self) -> u32 {
        self.config.sample_rate()
    }

    /// The device channels the stream delivers.
    pub fn channels(&self) -> usize {
        usize::from(self.config.channels())
    }

    /// Opens the stream at exactly this config and starts it, feeding `feed`, which must be
    /// built for [`PreparedInput::rate`] and [`PreparedInput::channels`]. Any sample format is
    /// converted to `f32` in a buffer allocated before the stream starts. The `Stream` must be
    /// kept alive; dropping it stops the capture.
    pub fn open(self, feed: Feed) -> Result<cpal::Stream, String> {
        let (name, device) = (&self.name, &self.device);
        let format = self.config.sample_format();
        let config = self.config.config();
        let errors = feed.errors();
        let stream = match format {
            SampleFormat::F32 => build_f32(device, config, feed, errors),
            SampleFormat::F64 => build_converted::<f64>(device, config, feed, errors),
            SampleFormat::I8 => build_converted::<i8>(device, config, feed, errors),
            SampleFormat::I16 => build_converted::<i16>(device, config, feed, errors),
            SampleFormat::I24 => build_converted::<I24>(device, config, feed, errors),
            SampleFormat::I32 => build_converted::<i32>(device, config, feed, errors),
            SampleFormat::I64 => build_converted::<i64>(device, config, feed, errors),
            SampleFormat::U8 => build_converted::<u8>(device, config, feed, errors),
            SampleFormat::U16 => build_converted::<u16>(device, config, feed, errors),
            SampleFormat::U24 => build_converted::<U24>(device, config, feed, errors),
            SampleFormat::U32 => build_converted::<u32>(device, config, feed, errors),
            SampleFormat::U64 => build_converted::<u64>(device, config, feed, errors),
            // DSD and any format added later.
            other => {
                return Err(format!(
                    "Could not open {name}: unsupported sample format {other}"
                ));
            }
        }
        .map_err(|e| format!("Could not open {name}: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("Could not start {name}: {e}"))?;
        Ok(stream)
    }
}

fn error_callback(errors: InputErrors) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e| errors.report(format!("Input device error: {e}"))
}

fn build_f32(
    device: &cpal::Device,
    config: StreamConfig,
    mut feed: Feed,
    errors: InputErrors,
) -> Result<cpal::Stream, cpal::Error> {
    device.build_input_stream::<f32, _, _>(
        config,
        move |data: &[f32], _| feed.push(data),
        error_callback(errors),
        None,
    )
}

/// Converts into a buffer allocated here, before the stream starts. Callbacks larger than the
/// buffer are processed in whole-frame pieces.
fn build_converted<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut feed: Feed,
    errors: InputErrors,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let chunk = SCRATCH_FRAMES * usize::from(config.channels).max(1);
    let mut scratch = vec![0.0f32; chunk];
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            for piece in data.chunks(chunk) {
                let tmp = &mut scratch[..piece.len()];
                for (o, s) in tmp.iter_mut().zip(piece) {
                    *o = f32::from_sample_(*s);
                }
                feed.push(tmp);
            }
        },
        error_callback(errors),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an audio device, so it only runs by hand:
    /// `cargo test -p limewave -- --ignored lists_input_devices_on_this_machine --nocapture`
    #[test]
    #[ignore]
    fn lists_input_devices_on_this_machine() {
        let devices = input_devices();
        for d in &devices {
            println!("{d:?}");
        }
        println!("{} input device(s)", devices.len());
    }
}
