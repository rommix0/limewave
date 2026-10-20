//! Manual check: plays a one-second 440 Hz tone through the default output device.

use std::f32::consts::TAU;
use std::thread::sleep;
use std::time::Duration;

use limewave::audio::engine::{AudioEngine, Command};
use limewave::audio::voice::PlayBuffer;
use limewave_core::edit::LoopMode;

fn main() {
    let mut engine = match AudioEngine::start() {
        Ok(engine) => engine,
        Err(e) => return eprintln!("audio unavailable: {e}"),
    };
    println!(
        "playing on {} at {} Hz",
        engine.device_name, engine.device_rate
    );
    let rate = 48_000;
    let sine = (0..rate).map(|i| 0.3 * (TAU * 440.0 * i as f32 / rate as f32).sin());
    engine.set_buffer(PlayBuffer {
        audio: vec![sine.collect()],
        rate,
        start: 0,
        end: rate as usize - 1,
        loop_mode: LoopMode::Off,
        loop_start: 0,
        loop_end: 0,
        root: 60,
        cents: 0.0,
    });
    engine.send(Command::NoteOn { note: 60, from: 0 });
    sleep(Duration::from_millis(1000));
    engine.send(Command::NoteOff { note: 60 });
    sleep(Duration::from_millis(100));
    println!("done");
}
