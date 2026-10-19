//! MIDI input: note-on/off messages become [`Command`]s for the audio thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use midir::{MidiInput, MidiInputConnection};

use super::engine::Command;

const CLIENT_NAME: &str = "Seamlime";

/// Names of the available MIDI input ports, in port-index order. Empty if MIDI is unavailable.
pub fn list_midi_ports() -> Vec<String> {
    let Ok(input) = MidiInput::new(CLIENT_NAME) else {
        return Vec::new();
    };
    input
        .ports()
        .iter()
        .map(|p| {
            input
                .port_name(p)
                .unwrap_or_else(|_| "Unknown port".to_string())
        })
        .collect()
}

/// Opens input port `index`. Each note message is pushed to `tx` (dropped if the queue is full);
/// a NoteOn starts from the frame currently stored in `start_frame`. The connection stays open
/// while the returned value is alive.
pub fn connect_midi(
    index: usize,
    mut tx: rtrb::Producer<Command>,
    start_frame: Arc<AtomicUsize>,
) -> Result<MidiInputConnection<()>, String> {
    let input = MidiInput::new(CLIENT_NAME).map_err(|e| format!("MIDI unavailable: {e}"))?;
    let ports = input.ports();
    let port = ports
        .get(index)
        .ok_or_else(|| format!("MIDI port {index} not found"))?;
    input
        .connect(
            port,
            "Seamlime input",
            move |_, msg, _| {
                if let Some(cmd) = parse_midi(msg, &start_frame) {
                    let _ = tx.push(cmd);
                }
            },
            (),
        )
        .map_err(|e| format!("Could not open MIDI port {index}: {e}"))
}

/// Maps a raw MIDI message to a command. Note-on with velocity 0 is a note-off; every other
/// message is ignored.
fn parse_midi(msg: &[u8], start_frame: &AtomicUsize) -> Option<Command> {
    let &[status, note, velocity, ..] = msg else {
        return None;
    };
    let note = note & 0x7f;
    match status & 0xf0 {
        0x90 if velocity > 0 => Some(Command::NoteOn {
            note,
            from: start_frame.load(Ordering::Relaxed),
        }),
        0x80 | 0x90 => Some(Command::NoteOff { note }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_note_messages_map_to_commands() {
        let start = AtomicUsize::new(1234);
        assert_eq!(
            parse_midi(&[0x93, 60, 100], &start),
            Some(Command::NoteOn {
                note: 60,
                from: 1234
            })
        );
        assert_eq!(
            parse_midi(&[0x90, 61, 0], &start),
            Some(Command::NoteOff { note: 61 })
        );
        assert_eq!(
            parse_midi(&[0x8f, 62, 64], &start),
            Some(Command::NoteOff { note: 62 })
        );
    }

    #[test]
    fn midi_other_messages_are_ignored() {
        let start = AtomicUsize::new(0);
        assert_eq!(parse_midi(&[0xb0, 7, 100], &start), None);
        assert_eq!(parse_midi(&[0xe0, 0, 64], &start), None);
        assert_eq!(parse_midi(&[0xc0, 5], &start), None);
        assert_eq!(parse_midi(&[0x90, 60], &start), None);
        assert_eq!(parse_midi(&[0xf8], &start), None);
        assert_eq!(parse_midi(&[], &start), None);
    }
}
