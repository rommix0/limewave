# Limewave

Limewave is an open-source loop and one-shot WAV sample editor, written in Rust. Open a WAV file or a folder of them, set the loop points, root key and fine tune by hand, listen to the result while you edit, and save files that samplers read correctly. It runs on Windows, macOS and Linux.

What it does in version 0.3:

- Reads and writes PCM (8/16/24/32-bit) and 32/64-bit float WAV files, mono or stereo, at any sample rate.
- Reads the loop, root key and fine tune stored in the file's `smpl` chunk. Forward, ping-pong and backward loops are supported.
- Waveform view with zoom, scroll and an overview strip; draggable Start, Loop start, Loop end and End markers with optional zero-crossing snap. Drag a marker by its label as well as its line, or double-click the label to type an exact frame.
- A seam view that overlays the audio before the loop end on the audio after the loop start, so you can see a bad splice before you hear it, with nudge buttons and a "lock length" option.
- Loop crossfade (equal-power or linear), previewed live and rendered into the audio on save. Crossfade is switched off for backward loops.
- Selection and the **Edit** menu: drag on the waveform to select (Shift-click extends an existing selection, or makes one from the play-from point), then play the selection, set the loop to the selection, or set Start and End to the selection. **Crop to selection** removes the audio outside the selection right away (undoable); a loop crossing its edges is clipped to it. **Trim after loop end** and **Crop to loop** set the trims from the loop points, with an adjustable padding.
- The **Effects** menu: gain, normalize, fade in and out (linear, exponential or S-curve), invert polarity, remove DC offset, reverse, stereo to mono (keep the left or right channel, or mix them), swap left and right, mono to stereo, and auto-trim silence. Effects work on the selection, or on the whole file or Start to End when nothing is selected. Each is one undo step, and the effects run before the crossfade and the trim when you save.
- **Generate tail…** (in the **Effects** menu): cut the sample where the audio you want ends (the End marker) and Limewave replaces everything after it with a rendered tail, baked in on save. Two modes: **Decay** fades the sound out like a real ending (the decay time is fitted from the audio before the cut, or set by hand), and **Sustain** builds a steady body that loops seamlessly, followed by a release. Three methods: **Smooth** (spectral freeze, suggested for tonal sounds), **Harmonic** (partial resynthesis) and **Granular** (patches of the source stitched together, suggested for noisy sounds). A **Darken** control makes the highs fade faster, and the dialog suggests a mode and method from an analysis of the source. It previews the result before you apply it, with the audio from before the cut leading in, and **Play original** lets you compare with the untouched file.
- **Make loop from spectrum…** (in the **Effects** menu): make a seamless loop of 0.1 to 30 s from a range of the sample (the selection, or the whole file) by rebuilding it from its spectrum. **Steady** keeps the pitch and holds a constant level, **Ensemble** widens each harmonic by a Detune amount for a swirling, chorused sound, and **Plain** follows the average spectrum with no pitch. An entry transition glides from the original audio into the loop and an exit transition glides back out, so it sits inside a file. The loop is set as loop 0, forward, and needs no crossfade.
- Generated audio is tinted lime in the waveform, so you can see which audio is synthetic. A tail or a spectrum loop is one undo step. Rendering happens in the background, and the same settings always give the same audio (**New variation** in Ensemble and Plain gives a different one).
- Live audition through your sound card, from the computer keyboard, an on-screen keyboard or a MIDI keyboard. Loop edits are heard while notes are sounding.
- An ADSR envelope (attack, decay, sustain, release) for the audition notes. Stopping a note releases it instead of cutting it. **Release plays after loop end** (in the envelope section, off by default) makes a released looped note leave the loop and play on through the audio after the loop end, as a sampler with a sustain loop does; the envelope's release still applies, so a long release lets the whole tail be heard. The envelope and this setting are remembered between runs; they are not saved into WAV files.
- Playback cursors in the waveform, the overview and the seam view show where each sounding note is. A click on the waveform sets the point the **Play** button starts from.
- Right-click a key on the on-screen keyboard to set the root key. With a MIDI keyboard connected, "Set from MIDI" sets the root key from the next note you play.
- The window size and position are saved between runs by eframe's standard persistence, which also keeps egui's own layout memory, such as panel sizes. This has not yet been checked by hand.
- Undo and redo for every edit.

*Screenshot coming soon.*

## Building and running

You need a recent stable Rust toolchain (https://rustup.rs).

```
cargo run --release -p limewave
```

To run the tests: `cargo test --workspace`.

To try the tail generator on a real sample from the command line, without the app, use the `tail_lab` bench. It cuts a file, renders a tail with each method you ask for, and writes the results (with the loop in the `smpl` chunk for sustain tails) into `target/tail-lab/<name>/` for listening:

```
cargo run --release -p limewave-core --example tail_lab -- <file.wav> --cut <seconds> --mode decay|sustain --method smooth|harmonic|granular|all [--body s] [--decay s] [--release s] [--darken 0..1] [--seed n] [--out dir]
```

It also prints the join, level and wave-shape measurements for each tail, and the render time.

On Linux, install the development packages for audio, MIDI and the window system first. On Debian and Ubuntu:

```
sudo apt-get install -y libasound2-dev libudev-dev libxkbcommon-dev
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
| Ctrl/Cmd+A | Select Start…End |
| Escape | Clear the selection and the play-from point (or cancel "Set from MIDI"). With a menu open, Escape only closes the menu. |
| Ctrl/Cmd+S | Save |
| Ctrl/Cmd+Shift+S | Save as… |
| Hold Shift while dragging a marker | Turn zero-crossing snap off for that drag |
| Shift-click on the waveform | Extend an existing selection, or make one from the play-from point |
| Mouse wheel over the waveform | Zoom around the cursor |
| Drag the overview box | Scroll |
| Drag either edge of the overview box | Zoom, keeping the other edge in place |

Note names follow the convention that MIDI note 60 is C4.

## What gets written when you save

- The audio is saved in the file's original format (same bit depth and sample rate). Only the frames between the Start and End markers are kept.
- A `smpl` chunk holds the root key, fine tune and loop. The loop end is stored inclusive, and the fine tune is stored the way the RIFF definition describes it (a pitch fraction upward from the unity note). With the loop off, the chunk is written with zero loops so the root key and tune still travel with the file. If a file had no `smpl` chunk and you changed nothing about tuning or looping, none is added.
- A `LIST/INFO` chunk with `ISFT` set to `Limewave <version>`. Other INFO fields (such as the name) are kept.
- Every chunk Limewave does not edit is kept, including cue markers and their labels (`cue ` and `LIST/adtl`). Cue positions follow trims and reverses, and cues that fall outside the saved range are dropped with their labels. The `fact` chunk follows the saved frame count.
- Effects, a generated tail or spectrum loop, a crossfade and trims are baked into the saved audio. A tail replaces everything after the End marker, and a spectrum loop replaces the range it was made from, so the saved file can be longer or shorter than the original; the `fact` chunk, cue positions and later loops follow. The save dialog lists any extra `smpl` loops that a tail or spectrum loop replaced, since those cannot be kept. A WAV file does not record that they were applied, so a saved file reopens with a crossfade of 0 and no effects to undo.
- Other chunks are copied through unchanged, in their original order. When you change the root key or fine tune, existing `inst` and `acid` chunks have their root note (and, for `inst`, fine tune) updated to agree with the `smpl` chunk; otherwise they are left as they are. A file without a `smpl` chunk opens with the root key and fine tune of its `inst` chunk, or else the root key of its `acid` chunk when that chunk marks it as set.
- A file you open and save with no edits comes back byte for byte, apart from the `ISFT` tag.

Where files go:

- By default a save writes into a new `limewave` folder next to the source file, keeping the file name. Your originals are not touched.
- Originals are modified only if you choose **Overwrite originals**. Each file is first copied to `<name>.wav.bak` (or `.1.bak`, `.2.bak` and so on if that exists); if the backup fails, that file is not saved.
- A file that Limewave itself wrote into its output folder, and then reopened, is replaced in place when you save it again.
- Limewave asks before replacing any other existing file.
- Files are written to a temporary file and renamed into place, so a failed save never leaves a half-written file.
- After a save, the open document continues from the saved audio and the undo history starts fresh.

## Checking tuning in your sampler

A small pack of test files lets you confirm that tuning and looping come out right in the sampler of your choice:

```
cargo run -p limewave-core --example check_pack --features testwav -- <out dir>
```

This writes three 48 kHz, 16-bit mono files, each with root note 69 (A4): `sine_A4_plus25c.wav` and `sine_A4_minus25c.wav` (sines 25 cents above and below 440 Hz, tuned by +25 and -25 cents) and `sine_A4_loop.wav` (a 440 Hz sine with an exact-period loop). Each should play in tune at A4, and the last should loop without a click.

## Known limitations

- Cue markers are kept in the file but Limewave does not show or edit them.
- The envelope is for audition only. It is not saved into WAV files.
- A backward loop has no crossfade.
- Many samplers ignore backward loops, so check one in yours before relying on it.
- When you switch files quickly, only the most recent release tail keeps sounding.
- Tails are rebuilt from the audio before the cut, so a sound whose ending differs from its body cannot be predicted. The known weak case is a low drum whose pitch falls: it is not detected as tonal, and its tail fades more slowly than the real ending would. Set the decay time by ear.
- The Harmonic method's sustain body drifts in level more than the other two (about 3.6 dB on one real tonal sample, against about 1 dB for Granular and 0.6 dB for Smooth). Listen before relying on it for a sustained loop.
- Spectrum loops at very low pitches and high sample rates take a few seconds to render. They render in the background, so the window stays usable.
- A tail or spectrum loop is stored as the audio it rendered. To change its settings, undo it and apply it again.

## Roadmap

1. **SFZ instruments:** a region list and key-by-velocity map, saving `.sfz` with its WAVs, opening existing `.sfz` files.
2. **Pitch detection:** detect the root key and fine tune, with octave fixes and root keys read from file names.
3. **Classifier:** tell one-shots, truncated samples, sustained sounds and rhythmic material apart, and suggest sensible editor defaults.
4. **Auto loop finder:** ranked loop candidates found from self-similarity and refined at sample level, shown beside the seam view.
5. **Beat slicing:** cut rhythmic loops into hits at detected onsets, with a rendered tail for each slice.

Possible later: batch processing over a folder, a command-line tool, a CLAP plugin.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option.
