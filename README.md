# Seamlime

Seamlime is an open-source loop and one-shot WAV sample editor, written in Rust. Open a WAV file or a folder of them, set the loop points, root key and fine tune by hand, listen to the result while you edit, and save files that samplers read correctly. It runs on Windows, macOS and Linux.

What it does in version 0.1:

- Reads and writes PCM (8/16/24/32-bit) and 32/64-bit float WAV files, mono or stereo, at any sample rate.
- Reads the loop, root key and fine tune stored in the file's `smpl` chunk.
- Waveform view with zoom, scroll and an overview strip; draggable Start, Loop start, Loop end and End markers with optional zero-crossing snap.
- A seam view that overlays the audio before the loop end on the audio after the loop start, so you can see a bad splice before you hear it, with nudge buttons and a "lock length" option.
- Loop crossfade (equal-power or linear), previewed live and rendered into the audio on save.
- Live audition through your sound card, from the computer keyboard, an on-screen keyboard or a MIDI keyboard. Loop edits are heard while notes are sounding.
- Undo and redo for every edit.

*Screenshot coming soon.*

## Building and running

You need a recent stable Rust toolchain (https://rustup.rs).

```
cargo run --release -p seamlime
```

To run the tests: `cargo test --workspace`.

On Linux, install the development packages for audio, MIDI and the window system first. On Debian and Ubuntu:

```
sudo apt-get install -y libasound2-dev libudev-dev libgtk-3-dev libxkbcommon-dev
```

The application icon sources and PNGs live in `assets/icon`.

## Keyboard shortcuts

Notes play when the **Audition** toggle is on and no text field has focus.

| Keys | Action |
| --- | --- |
| Z S X D C V G B H N J M , | Lower row of notes (white keys on the bottom row, black keys on the row above). Starts at C3 by default. |
| Q 2 W 3 E R 5 T 6 Y 7 U I | Upper row of notes, one octave higher than the lower row. |
| - and = | Move both rows down or up one octave. |
| Ctrl/Cmd+Z | Undo |
| Ctrl/Cmd+Shift+Z or Ctrl/Cmd+Y | Redo |
| Ctrl/Cmd+S | Save |
| Ctrl/Cmd+Shift+S | Save as… |
| Hold Shift while dragging a marker | Turn zero-crossing snap off for that drag |
| Mouse wheel over the waveform | Zoom around the cursor |

Note names follow the convention that MIDI note 60 is C4.

## What gets written when you save

- The audio is saved in the file's original format (same bit depth and sample rate). Only the frames between the Start and End markers are kept.
- A `smpl` chunk holds the root key, fine tune and loop. The loop end is stored inclusive, and the fine tune is stored the way the RIFF definition describes it (a pitch fraction upward from the unity note). With the loop off, the chunk is written with zero loops so the root key and tune still travel with the file. If a file had no `smpl` chunk and you changed nothing about tuning or looping, none is added.
- A `LIST/INFO` chunk with `ISFT` set to `Seamlime <version>`. Other INFO fields (such as the name) are kept.
- Cue markers (`cue ` and `LIST/adtl` chunks) are removed. The inspector warns you when a file has them.
- A crossfade is baked into the saved audio. A WAV file does not record that a crossfade was applied, so a saved file reopens with a crossfade of 0.
- Other chunks are copied through unchanged, in their original order. Existing `inst` and `acid` chunks have their root note (and, for `inst`, fine tune) updated to agree with the `smpl` chunk.

Where files go:

- By default a save writes into a new `seamlime` folder next to the source file, keeping the file name. Your originals are not touched.
- Originals are modified only if you choose **Overwrite originals**. Each file is first copied to `<name>.wav.bak` (or `.1.bak`, `.2.bak` and so on if that exists); if the backup fails, that file is not saved.
- A file that Seamlime itself wrote into its output folder, and then reopened, is replaced in place when you save it again.
- Seamlime asks before replacing any other existing file.
- Files are written to a temporary file and renamed into place, so a failed save never leaves a half-written file.
- After a save, the open document continues from the saved audio and the undo history starts fresh.

## Checking tuning in your sampler

A small pack of test files lets you confirm that tuning and looping come out right in the sampler of your choice:

```
cargo run -p seamlime-core --example check_pack --features testwav -- <out dir>
```

This writes three 48 kHz, 16-bit mono files, each with root note 69 (A4): `sine_A4_plus25c.wav` and `sine_A4_minus25c.wav` (sines 25 cents above and below 440 Hz, tuned by +25 and -25 cents) and `sine_A4_loop.wav` (a 440 Hz sine with an exact-period loop). Each should play in tune at A4, and the last should loop without a click.

## Roadmap

1. **SFZ instruments:** a region list and key-by-velocity map, saving `.sfz` with its WAVs, opening existing `.sfz` files.
2. **Pitch detection:** detect the root key and fine tune, with octave fixes and root keys read from file names.
3. **Classifier:** tell one-shots, truncated samples, sustained sounds and rhythmic material apart, and suggest sensible editor defaults.
4. **Auto loop finder:** ranked loop candidates found from self-similarity and refined at sample level, shown beside the seam view.
5. **Tail rendering:** extend truncated samples with a rendered tail, baked in on save.

Possible later: batch processing over a folder, a command-line tool, a CLAP plugin.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option.
