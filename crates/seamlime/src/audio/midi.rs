//! MIDI input: note-on/off messages become [`Command`]s for the audio thread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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

type Slot = Mutex<Option<rtrb::Producer<Command>>>;

/// An open MIDI input port. The port stays open while this value is alive.
///
/// The command producer is held in a shared slot rather than handed to midir as connection data.
/// midir drops that data when `connect` fails, and this way a failed connection can still give
/// the producer back.
pub struct MidiConnection {
    conn: MidiInputConnection<()>,
    slot: Arc<Slot>,
}

impl MidiConnection {
    /// Closes the port and returns the command producer so it can be connected again.
    pub fn close(self) -> rtrb::Producer<Command> {
        let slot = self.slot;
        drop(self.conn);
        take(&slot)
    }
}

/// Opens input port `index`. Each note message is pushed to `tx` (dropped if the queue is full);
/// a NoteOn starts from the frame currently stored in `start_frame`.
///
/// On failure the error message comes back together with `tx`. To switch ports, call
/// [`MidiConnection::close`] on the old connection to get the producer back, then call
/// `connect_midi` again with it.
pub fn connect_midi(
    index: usize,
    tx: rtrb::Producer<Command>,
    start_frame: Arc<AtomicUsize>,
) -> Result<MidiConnection, (String, rtrb::Producer<Command>)> {
    let input = match MidiInput::new(CLIENT_NAME) {
        Ok(input) => input,
        Err(e) => return Err((format!("MIDI unavailable: {e}"), tx)),
    };
    let ports = input.ports();
    let Some(port) = ports.get(index) else {
        return Err((format!("MIDI port {index} not found"), tx));
    };
    let slot = Arc::new(Mutex::new(Some(tx)));
    let cb_slot = Arc::clone(&slot);
    match input.connect(
        port,
        "Seamlime input",
        move |_, msg, _| forward(&cb_slot, msg, &start_frame),
        (),
    ) {
        Ok(conn) => Ok(MidiConnection { conn, slot }),
        Err(e) => Err((
            format!("Could not open MIDI port {index}: {e}"),
            take(&slot),
        )),
    }
}

/// The MIDI callback body. It never blocks: if the slot is busy or already taken, or the queue
/// is full, the message is dropped.
fn forward(slot: &Slot, msg: &[u8], start_frame: &AtomicUsize) {
    let Some(cmd) = parse_midi(msg, start_frame) else {
        return;
    };
    if let Ok(mut guard) = slot.try_lock()
        && let Some(tx) = guard.as_mut()
    {
        let _ = tx.push(cmd);
    }
}

/// Removes the producer from the slot. Only `connect_midi`'s error path and
/// `MidiConnection::close` call this, each consuming the connection, so it runs at most once
/// per slot.
fn take(slot: &Slot) -> rtrb::Producer<Command> {
    slot.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("the MIDI producer is taken once per connection")
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
    fn midi_failed_connect_returns_the_producer() {
        let (tx, mut rx) = rtrb::RingBuffer::new(4);
        let start = Arc::new(AtomicUsize::new(0));
        let Err((msg, mut tx)) = connect_midi(usize::MAX, tx, start) else {
            panic!("port usize::MAX cannot exist");
        };
        assert!(!msg.is_empty());
        tx.push(Command::AllOff).unwrap();
        assert_eq!(rx.pop(), Ok(Command::AllOff));
    }

    #[test]
    fn midi_slot_forwards_until_taken() {
        let (tx, mut rx) = rtrb::RingBuffer::new(4);
        let slot = Mutex::new(Some(tx));
        let start = AtomicUsize::new(7);
        forward(&slot, &[0x90, 60, 1], &start);
        assert_eq!(rx.pop(), Ok(Command::NoteOn { note: 60, from: 7 }));
        let mut tx = take(&slot);
        // Once taken, messages are dropped instead of panicking.
        forward(&slot, &[0x80, 60, 0], &start);
        assert!(rx.pop().is_err());
        tx.push(Command::AllOff).unwrap();
        assert_eq!(rx.pop(), Ok(Command::AllOff));
    }

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
