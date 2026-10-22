//! The Record panel: records takes from an input device, each into a new untitled file.
//!
//! The input is open only while the panel is: opening it opens the default device, and closing
//! it closes the input. There is no monitoring; the meter shows the level.

use std::time::{Duration, Instant};

use eframe::egui::{self, Button, Color32, ComboBox, Role, Sense, Ui, WidgetInfo, vec2};

use crate::app::LimewaveApp;
use crate::audio::input::{InputDevice, input_devices, prepare_input};
#[cfg(test)]
use crate::audio::record::Feed;
use crate::audio::record::{
    CAP_SECONDS, InputChoice, Recorder, Take, default_choice, input_choices,
};
use crate::theme::colors;
use crate::ui::dialog_kit::PLOT_BACKGROUND;
use crate::ui::new_dialog::BitDepth;

/// The bottom of the meter, in dBFS; its top is 0 dBFS.
const METER_FLOOR_DB: f32 = -60.0;
/// How long the meter holds a peak before it falls to the current level.
const METER_HOLD: Duration = Duration::from_millis(500);
/// How often the panel repaints, so the meter moves.
const METER_REPAINT: Duration = Duration::from_millis(30);
/// The clip light while it is off.
const CLIP_OFF: Color32 = Color32::from_rgb(0x3a, 0x3f, 0x4a);

/// Shown when the host lists no input device.
pub const NO_INPUT: &str = "No audio input device found";

/// The Record panel's state, while it is open.
pub struct RecordPanelState {
    /// The input devices, the system default first, as listed when the panel opened.
    pub devices: Vec<InputDevice>,
    pub device: usize,
    pub choice: InputChoice,
    pub bits: BitDepth,
    /// The open input. Declared before `recorder` so it drops first: the callback stops
    /// feeding before the capture thread shuts down.
    pub stream: Option<cpal::Stream>,
    pub recorder: Option<Recorder>,
    /// Why the device did not open; Record stays disabled until one does.
    pub error: Option<String>,
    /// Why the last take stopped by itself.
    pub notice: Option<String>,
    /// Close was pressed while recording: "Stop and keep the take?" is up.
    pub confirm_close: bool,
    /// One per chosen channel.
    holds: Vec<MeterHold>,
}

impl RecordPanelState {
    /// Lists the input devices and opens the default one with its default inputs. If that
    /// fails, `error` says why.
    pub fn open() -> RecordPanelState {
        let devices = input_devices();
        let choice = default_choice(devices.first().map_or(1, |d| d.channels));
        let mut panel = RecordPanelState::closed(devices, choice);
        // A new panel has no input yet, so no take.
        let _ = panel.reopen();
        panel
    }

    fn closed(devices: Vec<InputDevice>, choice: InputChoice) -> RecordPanelState {
        RecordPanelState {
            devices,
            device: 0,
            choice,
            bits: BitDepth::default(),
            stream: None,
            recorder: None,
            error: None,
            notice: None,
            confirm_close: false,
            holds: Vec::new(),
        }
    }

    /// A panel recording `choice` of a `device_channels`-channel device at `rate`, with no
    /// stream: the test feeds the returned [`Feed`] itself.
    #[cfg(test)]
    pub(crate) fn with_recorder(
        rate: u32,
        device_channels: usize,
        choice: InputChoice,
    ) -> (RecordPanelState, Feed) {
        let device = InputDevice {
            name: "Test input".into(),
            channels: device_channels,
            rate,
            is_default: true,
        };
        let mut panel = RecordPanelState::closed(vec![device], choice);
        let (recorder, feed) = Recorder::new(rate, device_channels, choice);
        panel.recorder = Some(recorder);
        (panel, feed)
    }

    /// Closes the input, collecting any take it still holds; returns that take.
    fn close_input(&mut self) -> Option<Take> {
        let take = self.recorder.as_mut().and_then(Recorder::stop);
        self.stream = None;
        self.recorder = None;
        self.holds.clear();
        take
    }

    /// Closes the input, then opens the chosen inputs of the chosen device at the rate and
    /// channel count it opens at now, which may have changed since the devices were listed.
    /// Returns any take the old input still held, for the caller to keep.
    #[must_use]
    fn reopen(&mut self) -> Option<Take> {
        let take = self.close_input();
        let Some(device) = self.devices.get(self.device) else {
            self.error = Some(NO_INPUT.into());
            return take;
        };
        let input = match prepare_input(&device.name) {
            Ok(input) => input,
            Err(e) => {
                self.error = Some(e);
                return take;
            }
        };
        self.adopt_format(input.rate(), input.channels());
        let (recorder, feed) = Recorder::new(input.rate(), input.channels(), self.choice);
        match input.open(feed) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.recorder = Some(recorder);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        take
    }

    /// The chosen device opens at `rate` with `channels` inputs: shows them, and falls back to
    /// the device's default inputs when the chosen ones no longer exist.
    fn adopt_format(&mut self, rate: u32, channels: usize) {
        if let Some(device) = self.devices.get_mut(self.device) {
            device.rate = rate;
            device.channels = channels;
        }
        if !input_choices(channels).contains(&self.choice) {
            self.choice = default_choice(channels);
        }
    }

    /// The device failed during a take: closes the input and shows `text` as the error, so
    /// Record stays disabled until a device opens again. Returns any take the input still
    /// held, for the caller to keep.
    #[must_use]
    pub(crate) fn device_failed(&mut self, text: String) -> Option<Take> {
        let take = self.close_input();
        self.error = Some(text);
        self.notice = None;
        take
    }

    fn recording(&self) -> bool {
        self.recorder.as_ref().is_some_and(Recorder::recording)
    }

    pub(crate) fn can_record(&self) -> bool {
        self.error.is_none() && self.recorder.is_some()
    }

    /// The device's rate, which is the take's.
    fn rate(&self) -> Option<u32> {
        self.devices.get(self.device).map(|d| d.rate)
    }
}

/// A meter bar's level, held for [`METER_HOLD`] after a peak.
#[derive(Clone, Copy, Debug)]
struct MeterHold {
    level: f32,
    since: Instant,
}

impl MeterHold {
    /// Shows `level`, unless a higher one was shown less than [`METER_HOLD`] ago.
    fn update(&mut self, level: f32, now: Instant) -> f32 {
        if level >= self.level || now.duration_since(self.since) >= METER_HOLD {
            self.level = level;
            self.since = now;
        }
        self.level
    }
}

/// Where linear `peak` sits on the meter: 0 at -60 dBFS and below, 1 at 0 dBFS and above.
fn meter_fraction(peak: f32) -> f32 {
    if peak <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * peak.log10();
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

/// `seconds` as `m:ss`.
fn mmss(seconds: usize) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The Record panel, while `app.ui.record_panel` is set. Each finished take becomes a new
/// untitled file and the panel stays open for the next. Close, Escape and a click outside
/// close it, except while recording: then Close asks whether to keep the take.
pub fn record_panel(ctx: &egui::Context, app: &mut LimewaveApp) {
    let Some(mut p) = app.ui.record_panel.take() else {
        return;
    };
    let mut takes = Vec::new();
    // Read before the poll: once a take is seen stopped, the poll collects it, so Record (shown
    // only then) never starts a new take over a finished one.
    let recording = p.recording();
    takes.extend(p.recorder.as_mut().and_then(Recorder::poll));
    if !recording {
        p.confirm_close = false;
    }
    let mut close = false;
    let mut discard = false;
    let modal = egui::Modal::new(egui::Id::unique("record_panel")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.heading("Record");
        ui.add_space(4.0);
        ui.add_enabled_ui(!recording, |ui| takes.extend(choosers(ui, &mut p)));
        ui.horizontal(|ui| {
            let label = ui.label("Bit depth");
            ComboBox::from_id_salt("record_bits")
                .selected_text(p.bits.label())
                .show_ui(ui, |ui| {
                    for b in BitDepth::ALL {
                        ui.selectable_value(&mut p.bits, b, b.label());
                    }
                })
                .response
                .labelled_by(label.id);
            if let Some(rate) = p.rate() {
                ui.label(format!("{rate} Hz"));
            }
        });
        ui.add_space(4.0);
        meters(ui, &mut p);
        let frames = p.recorder.as_ref().map_or(0, Recorder::recorded_frames);
        let elapsed = frames / p.rate().unwrap_or(1).max(1) as usize;
        ui.label(format!(
            "{} recorded, {} left",
            mmss(elapsed),
            mmss(CAP_SECONDS.saturating_sub(elapsed))
        ));
        if let Some(error) = &p.error {
            ui.colored_label(colors::END, error);
        }
        if let Some(notice) = &p.notice {
            ui.colored_label(colors::LOOP, notice);
        }
        ui.add_space(8.0);
        if p.confirm_close {
            ui.label("Stop and keep the take?");
            ui.horizontal(|ui| {
                // Closing keeps the take (below).
                close |= ui.button("Keep").clicked();
                if ui.button("Discard").clicked() {
                    discard = true;
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    p.confirm_close = false;
                }
            });
        } else {
            ui.horizontal(|ui| {
                if recording {
                    if ui.button("Stop").clicked() {
                        takes.extend(p.recorder.as_mut().and_then(Recorder::stop));
                    }
                } else if ui
                    .add_enabled(p.can_record(), Button::new("Record"))
                    .clicked()
                    && let Some(recorder) = &mut p.recorder
                {
                    p.notice = None;
                    recorder.start();
                }
                if ui.button("Close").clicked() {
                    if recording {
                        p.confirm_close = true;
                    } else {
                        close = true;
                    }
                }
            });
        }
    });
    // Escape or a click outside does nothing while a take was recording at the frame's start.
    if modal.should_close() && !recording && !p.recording() {
        close = true;
    }
    ctx.request_repaint_after(METER_REPAINT);
    let bits = p.bits;
    if close {
        // Every way of closing but Discard keeps the take in progress, or one the capture
        // thread stopped by itself since the poll above. Closing the input drops the rest.
        let take = p.close_input();
        if !discard {
            takes.extend(take);
        }
    } else {
        app.ui.record_panel = Some(p);
    }
    for take in takes {
        app.add_recording(take, bits);
    }
}

/// The Device and Input choosers. Changing either reopens the input; while an error shows,
/// choosing the current one again retries it. Returns any take the old input still held.
fn choosers(ui: &mut Ui, p: &mut RecordPanelState) -> Option<Take> {
    let mut device = None;
    ui.horizontal(|ui| {
        let label = ui.label("Device");
        let current = p.devices.get(p.device).map_or("", |d| d.name.as_str());
        ComboBox::from_id_salt("record_device")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (i, d) in p.devices.iter().enumerate() {
                    if ui.selectable_label(i == p.device, &d.name).clicked() {
                        device = Some(i);
                    }
                }
            })
            .response
            .labelled_by(label.id);
    });
    let channels = p.devices.get(p.device).map_or(1, |d| d.channels);
    let mut choice = None;
    ui.horizontal(|ui| {
        let label = ui.label("Input");
        ComboBox::from_id_salt("record_input")
            .selected_text(p.choice.label(channels))
            .show_ui(ui, |ui| {
                for c in input_choices(channels) {
                    if ui
                        .selectable_label(c == p.choice, c.label(channels))
                        .clicked()
                    {
                        choice = Some(c);
                    }
                }
            })
            .response
            .labelled_by(label.id);
    });
    let retry = p.error.is_some();
    if let Some(i) = device.filter(|&i| i != p.device || retry) {
        p.device = i;
        p.choice = default_choice(p.devices[i].channels);
        p.reopen()
    } else if let Some(c) = choice.filter(|&c| c != p.choice || retry) {
        p.choice = c;
        p.reopen()
    } else {
        None
    }
}

/// One meter row per chosen channel: the level in dBFS from -60 to 0, and a clip light that
/// a click clears.
fn meters(ui: &mut Ui, p: &mut RecordPanelState) {
    let Some(recorder) = &p.recorder else {
        return;
    };
    let now = Instant::now();
    let meter = recorder.meter();
    p.holds.resize_with(meter.len(), || MeterHold {
        level: 0.0,
        since: now,
    });
    let stereo = meter.len() == 2;
    for (c, ((peak, clip), hold)) in meter.into_iter().zip(&mut p.holds).enumerate() {
        let level = hold.update(meter_fraction(peak), now);
        ui.horizontal(|ui| {
            ui.label(match (stereo, c) {
                (false, _) => "M",
                (true, 0) => "L",
                (true, _) => "R",
            });
            let (rect, response) = ui.allocate_exact_size(vec2(300.0, 12.0), Sense::hover());
            response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, "Level"));
            let painter = ui.painter();
            painter.rect_filled(rect, 2.0, PLOT_BACKGROUND);
            let mut bar = rect;
            bar.set_width(rect.width() * level);
            painter.rect_filled(bar, 2.0, colors::LIME);

            let (light, response) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::click());
            let name = format!("Clip {}", c + 1);
            response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &name));
            let color = if clip { colors::END } else { CLIP_OFF };
            ui.painter().circle_filled(light.center(), 5.0, color);
            if response
                .on_hover_text("Lights at -0.1 dBFS or above; click to clear")
                .clicked()
            {
                recorder.clear_clip(c);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::record::InputChoice::{Mono, Pair};

    #[test]
    fn times_show_as_minutes_and_seconds() {
        assert_eq!(mmss(0), "0:00");
        assert_eq!(mmss(61), "1:01");
        assert_eq!(mmss(CAP_SECONDS), "10:00");
    }

    #[test]
    fn the_meter_spans_minus_60_to_0_dbfs() {
        assert_eq!(meter_fraction(0.0), 0.0);
        assert_eq!(meter_fraction(0.000_5), 0.0, "below the floor");
        assert!(meter_fraction(0.001).abs() < 1e-4, "-60 dBFS");
        assert!((meter_fraction(0.1) - 2.0 / 3.0).abs() < 1e-4, "-20 dBFS");
        assert_eq!(meter_fraction(1.0), 1.0);
        assert_eq!(meter_fraction(2.0), 1.0, "above full scale");
    }

    #[test]
    fn the_meter_holds_a_peak_briefly() {
        let t0 = Instant::now();
        let mut hold = MeterHold {
            level: 0.0,
            since: t0,
        };
        assert_eq!(hold.update(0.8, t0), 0.8);
        let soon = t0 + Duration::from_millis(100);
        assert_eq!(hold.update(0.2, soon), 0.8, "held");
        let later = t0 + METER_HOLD;
        assert_eq!(hold.update(0.2, later), 0.2, "then it falls");
        assert_eq!(hold.update(0.9, later), 0.9, "a higher peak shows at once");
    }

    #[test]
    fn no_device_disables_record() {
        let (mut p, _feed) = RecordPanelState::with_recorder(48_000, 2, Pair(0));
        assert!(p.can_record());
        p.devices.clear();
        assert!(p.reopen().is_none());
        assert_eq!(p.error.as_deref(), Some(NO_INPUT));
        assert!(p.recorder.is_none());
        assert!(!p.can_record());
    }

    /// A panel whose capture thread stopped a 100-frame take by itself, not yet polled.
    fn panel_with_a_finished_take() -> RecordPanelState {
        let (mut p, mut feed) = RecordPanelState::with_recorder(48_000, 2, Pair(0));
        let recorder = p.recorder.as_mut().unwrap();
        recorder.start();
        feed.push(&[0.25; 200]);
        feed.errors().report("gone".into());
        let deadline = Instant::now() + Duration::from_secs(5);
        while recorder.recording() {
            assert!(
                Instant::now() < deadline,
                "the thread did not stop the take"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        p
    }

    #[test]
    fn reopening_hands_back_a_take_the_thread_finished() {
        let mut p = panel_with_a_finished_take();
        // No device, so nothing opens again: only the old input's take matters.
        p.devices.clear();
        let take = p.reopen().expect("the finished take");
        assert_eq!(take.frames(), 100);
        assert!(p.recorder.is_none());
    }

    #[test]
    fn a_failed_device_hands_back_a_take_the_thread_finished() {
        let mut p = panel_with_a_finished_take();
        let take = p.device_failed("gone".into()).expect("the finished take");
        assert_eq!(take.frames(), 100);
        assert_eq!(p.error.as_deref(), Some("gone"));
        assert!(p.recorder.is_none() && p.stream.is_none());
    }

    #[test]
    fn reopening_adopts_the_format_the_device_opens_at() {
        let (mut p, _feed) = RecordPanelState::with_recorder(48_000, 2, Pair(0));
        p.adopt_format(44_100, 1);
        assert_eq!((p.devices[0].rate, p.devices[0].channels), (44_100, 1));
        assert_eq!(p.rate(), Some(44_100), "the shown rate is the take's");
        assert_eq!(p.choice, Mono(0), "the pair no longer exists");

        p.choice = Mono(0);
        p.adopt_format(96_000, 4);
        assert_eq!(p.choice, Mono(0), "a choice that still fits is kept");
        p.choice = Pair(2);
        p.adopt_format(96_000, 2);
        assert_eq!(p.choice, Pair(0), "falls back to the default inputs");
    }
}
