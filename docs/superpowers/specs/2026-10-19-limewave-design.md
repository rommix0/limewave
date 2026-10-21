# Limewave: design

Status: draft for approval · 2026-10-19. v0.1 is built. Version 0.2 and the new milestone order are specified in `2026-10-20-limewave-v0.2-design.md`, which wins where the two disagree.

Limewave is an open-source, cross-platform editor for audio one-shots and loops. You open WAV files (and later SFZ instruments), set and audition loop points, root key and fine tune by hand, and save samplers-ready files. Later versions add analysis that does the tedious parts for you: pitch detection, a one-shot/sustain classifier, an automatic loop finder, and tail rendering for samples that were cut off.

This document covers the whole product direction and specifies **version 0.1** in enough detail to plan and build it. Later milestones are described at the level of intent and order; each gets its own design pass before it is built.

## 1. Intent

Looping a sample by hand is fiddly: the loop has to land on matching points in the waveform, a crossfade has to hide what doesn't match, and the root key has to be right or the sample plays out of tune across the keyboard. Existing tools that do this well are closed-source, old, or Windows-only. Limewave should be the tool a sound designer reaches for to prepare samples for a sampler: fast to open, precise at the sample level, and honest about what it writes into the file.

What makes it different:

- **The seam view.** A dedicated panel overlays the audio just before the loop end on the audio just after the loop start, so you can see a bad splice and fix it before you hear it.
- **Samplers-ready output.** Loop points, root key and fine tune are written into the WAV `smpl` chunk exactly as samplers expect, and later into SFZ instruments.
- **Analysis that suggests, never decides.** Later milestones propose loop points, pitch and sample type; the user can always override.
- **Open source and permissive.** Anyone can use, fork or embed it.

## 2. Goals, non-goals, anti-goals

### Goals (whole product)

1. Edit loop points on a sample, at sample precision, with live audition.
2. Set root key and fine tune; later, detect pitch automatically.
3. Save individual WAV files with `smpl` metadata, or an SFZ instrument that maps many samples across the keyboard.
4. Later: classify samples (complete one-shot / truncated / sustained / rhythmic), find good loops automatically, and render new tails for truncated samples.
5. Run on Windows, macOS and Linux from one codebase.

### Goals for v0.1

A hand-editing loop tool that someone would actually use on real samples:

1. Open one WAV file or a folder of WAV files.
2. Read existing loop points and root key from the `smpl` chunk.
3. Waveform view with zoom/scroll, an overview strip, and draggable Start / Loop start / Loop end / End markers with optional zero-crossing snap.
4. Seam view with nudge buttons and "lock length".
5. Audition the sample with its loop: play button plus computer-keyboard notes (MIDI keyboard input if it comes cheaply), hearing loop edits live while playing, without clicks from the editing itself.
6. Edit root key and fine tune by hand.
7. Loop crossfade (equal-power or linear), previewed live, rendered into the audio on save.
8. Save as individual WAV files with a `smpl` chunk and a `LIST/INFO ISFT` software tag.
9. Undo / redo for every edit.

### Non-goals for v0.1 (planned for later milestones)

- SFZ instruments: region list, key × velocity map, `.sfz` save/load.
- Pitch detection, the sample classifier, the auto loop finder, tail rendering.
- Sustain/release loop modes (these are SFZ playback behaviour, not WAV metadata).

### Not planned at all (for now)

- Other proprietary or third-party instrument formats.
- An audio plugin version (keep the core library free of UI and audio-device code so a CLAP plugin stays possible later).
- Recording, general audio effects, resampling, time-stretch.
- A browser version.

### Anti-goals (things that would make it a failure even if it "works")

- **Silently damaging the user's files.** Originals are never modified unless the user explicitly chooses to overwrite, and then a backup is kept (see §6.6).
- **Writing metadata that samplers misread.** An off-by-one loop end or wrongly encoded fine tune makes every exported file subtly broken. These encodings are specified exactly in §6.5 and must be covered by tests.
- **Clicks or dropouts caused by the editor itself** while dragging loop points during playback.
- **Dragging in copyleft dependencies.** No GPL/AGPL/LGPL-only crates or toolkits; the whole dependency tree must be permissively licensed (MIT, Apache-2.0, BSD, ISC, Zlib or similar).
- **Naming other commercial products** in the UI, docs, code comments or commit history. Describe features by what they do.

## 3. Constraints

- **Licence:** MIT OR Apache-2.0 dual licence (the Rust ecosystem convention). *My call; easy to change before the first public commit.*
- **Platforms:** Windows, macOS, Linux. No platform-specific UI code beyond what the libraries handle.
- **Language and toolkit:** Rust, with `egui` / `eframe` for the UI (decided).
- **Audio output:** `cpal`. **MIDI input:** `midir`. Both permissive.
- **No Node or Python at build or run time.** Python is allowed only for throwaway algorithm experiments in later milestones, never shipped.
- **Test data:** the files in `samples/` came from elsewhere (one appears to come from a commercial synth's sample set). They are useful for local manual testing but **must not be committed**; `samples/` is git-ignored. Automated tests generate their own synthetic WAVs.

## 4. Product identity

- **Name:** Limewave. Binary and crate prefix `limewave`.
- **Software tag written into WAVs:** `Limewave <version>` (version from `Cargo.toml`, e.g. `Limewave 0.1.0`).
- **Icon:** "A3": a wireframe lime slice in green outlines with a glowing neon-blue waveform across it, on a dark rounded tile. A simplified drawing (6 segments, thicker lines, no glow) is used at 16–32 px. Source SVGs: `icon-a3.svg` and `icon-a3-small.svg` (currently in the local `.superpowers/mockups/` folder; the builder copies them into the repo under `assets/icon/` and generates `.ico` / `.icns` / PNG sizes from them).
- **Wordmark colours:** "Lime" in green `#a3e635`, "wave" in neon blue `#22d3ee`, matching the icon's green lime and blue waveform.
- **UI theme:** dark, matching the mockups: panels around `#1b1e25`, waveform blue `#5fb3ff`, loop region and loop markers amber `#ffc400`, start marker green `#4ade80`, end marker red `#f87171`, crossfade purple `#c084fc`.

## 5. Window layout (v0.1)

The mockups in `.superpowers/mockups/` (local, git-ignored) show the target. `layout-v4-wav.html` is the closest to v0.1; `layout-v2-sfz.html` shows where the SFZ milestone takes it.

```
┌──────────────────────────────────────────────────────────────────────────┐
│ Toolbar: folder/file name · Save · Save as… · Undo/Redo · ▶ ⟲Loop ■ ·    │
│          Snap · Audition toggle                                           │
├───────────────┬───────────────────────────────────────┬──────────────────┤
│ File list     │ Overview strip (whole file + view box)│ Inspector        │
│ (folder mode) ├───────────────────────────────────────┤  · File metadata │
│  thumbnail    │ Main waveform                         │    (as read)     │
│  name ●unsaved│  ruler, Start/Loop/Loop/End markers,  │  · Tuning        │
│  root key     │  loop region tint, crossfade ramp     │  · Loop          │
│  chips: smpl, │                                       │  · Save button   │
│  looped/none  ├───────────────────┬───────────────────┤                  │
│               │ Seam view         │ (reserved: loop   │                  │
│               │  + nudge controls │  candidates, M4)  │                  │
├───────────────┴───────────────────┴───────────────────┴──────────────────┤
│ Keyboard strip: audition keys, root key highlighted                       │
└──────────────────────────────────────────────────────────────────────────┘
```

- **Single-file mode:** opening one WAV hides the file list (or shows a list of one).
- **Folder mode:** the list shows every `.wav` in the folder (not recursive in v0.1). Selecting a file loads it into the editor; edits to other files stay in memory and are marked unsaved (●).
- **The reserved panel** next to the seam view holds loop candidates in a later milestone. In v0.1 it may show loop length (samples, ms, and cycles once pitch is known) or simply be omitted and let the seam view take the width. *Builder's choice.*
- Panels should be resizable by dragging their borders.

## 6. v0.1 specification

### 6.1 Reading WAV files

- Formats: PCM integer 8/16/24/32-bit and IEEE float 32-bit (and 64-bit float if cheap), including `WAVE_FORMAT_EXTENSIBLE` wrappers of those. Any sample rate. Mono and stereo. Files with more than 2 channels are rejected with a clear message (not a crash).
- Audio is converted to `f32` in memory for display, playback and processing; the original bit depth and format are remembered for saving.
- Chunks:
  - `fmt `, `data`: required.
  - `smpl`: parsed (see §6.5). Multiple loops: v0.1 edits **loop 0** only and preserves any further loops unchanged.
  - `LIST/INFO`: parsed; `INAM` (name) and `ISFT` (software) shown in the inspector.
  - `cue ` and `LIST/adtl`: shown in the inspector as "markers found" but not edited (see §6.6 for what happens on save).
  - `inst` and `acid`: these repeat the root note (and `inst` the fine tune), so they are parsed. On save, if present, their root note / fine tune fields are updated to match the edit state so the file never holds two different answers. They are never added to a file that didn't have them. Other fields in them are kept. *My call.*
  - Any other chunk: kept byte-for-byte, in its original order, for saving.
- Malformed files (bad sizes, truncated `data`, odd-length chunks without padding) should be handled leniently where safe (clamp to file length) and reported, never panic.
- RIFF64/RF64 and files over 4 GB: not supported in v0.1; report clearly.

The RIFF reader/writer is custom code in the core library (it must round-trip unknown chunks and control `smpl` exactly). Using `hound` for sample decoding is allowed if convenient, but the chunk layer is ours.

### 6.2 Edit model

All editing is **non-destructive during the session.** The original decoded audio is kept unchanged; the edit state is a small set of parameters per file:

- start and end (inclusive frame indices into the original audio): the trim range. They default to the file's first and last frame. Playback starts at Start, and on save everything outside Start…End is cut (§6.6). Constraint: `start <= loop_start` and `loop_end <= end` whenever the loop is on; dragging a marker past that limit stops at it.
- loop enabled, loop mode (Off / Forward / Ping-pong), loop start, loop end (inclusive, in sample frames)
- crossfade length (frames) and shape (equal-power / linear)
- root key (MIDI note 0–127) and fine tune (cents, −50.0 … +50.0)

Undo/redo works on this parameter state (a stack of snapshots is fine; they're tiny). Dragging a marker is one undo step per drag, not per mouse-move.

The rendered audio (with crossfade applied) is produced on demand for audition and on save.

### 6.3 Waveform display and markers

- Peak data: a min/max pyramid per channel (for example 256-sample blocks, then repeated halving) built on load, so zoom and scroll stay smooth for files up to several minutes long.
- When zoomed in past roughly one sample per pixel, draw the actual samples as a line with dots at sample positions.
- Stereo: draw both channels (stacked lanes). Loop points are shared by both channels.
- Markers: Start (green), Loop start and Loop end (amber), End (red). Loop region tinted. Crossfade drawn as a ramp ending at the loop end.
- Drag to move markers. Shift-drag or a toolbar toggle temporarily disables snapping.
- **Zero-crossing snap** (toggle, on by default): a dragged loop marker snaps to the nearest upward zero crossing (sample `i` where `x[i-1] < 0 ≤ x[i]`, using the sum of channels for stereo), searched within a small window around the cursor. Exact rule for the loop end: the sample after the loop end (`end + 1`) should land on the upward crossing, so the jump from `end` to `start` continues the same slope. *Fine details of the search window are builder's choice.*
- "Lock length": when on, moving either loop marker moves both.
- Mouse wheel zooms around the cursor; a scrollbar or drag on the overview strip scrolls.
- Numeric fields in the inspector edit loop start/end directly.

### 6.4 Seam view

The core feature. A panel that shows, side by side at the same scale and centred on the seam:

- **left half:** the samples leading up to and including the loop end (solid, waveform colour), with the samples leading up to the loop start drawn faintly underneath (dashed)
- **right half:** the samples from the loop start onward (solid, loop colour), with the samples after the loop end drawn faintly underneath

If the loop is good, the solid line passes straight through the centre with no kink. Requirements:

- Zoom control for the window width (e.g. 16 to 2048 samples per side), sample dots when zoomed in.
- Nudge buttons for loop start and loop end: by 1 sample, and by a larger step (e.g. to the next/previous zero crossing).
- A small readout: the jump at the seam (value at loop end → value at loop start) compared with the natural next step (value at loop end → value at loop end + 1). This is a simple "how big is the click" hint; the scored match metric arrives with the loop finder.
- When a crossfade is set, the seam view shows the **crossfaded** result (what will be saved and heard).
- Stereo: show the channel sum, with an option to show L / R separately. *Builder's choice how.*

### 6.5 `smpl` chunk: exact encoding

Field layout follows the standard RIFF `smpl` chunk: manufacturer, product, sample period, MIDI unity note, MIDI pitch fraction, SMPTE format, SMPTE offset, number of loops, sampler data size, then 24-byte loop records (cue id, type, start, end, fraction, play count).

Writing rules:

- **Manufacturer, product:** 0, unless the file already had values; then keep them.
- **Sample period:** `round(1e9 / sample_rate)` nanoseconds.
- **Root key and fine tune:** the UI holds root key `n` and fine tune `c` in cents (−50 … +50). The file stores a unity note and an *upward-only* pitch fraction of a semitone. Convert so the pitch is the same:
  - if `c >= 0`: unity note = `n`, fraction = `round(c / 100 * 2^32)`
  - if `c < 0`: unity note = `n - 1`, fraction = `round((100 + c) / 100 * 2^32)`
  - Reading does the reverse and normalises back to the nearest note with −50 … +50 cents.
  - This follows the published RIFF definition: the fraction is "a fraction of a semitone up from the MIDI unity note", so the sample's recorded pitch is unity note plus the fraction. Implement and test against that text. Checking it against a real sampler is a nice-to-have manual step (§8), not a release gate. *My call.*
- **Loops:** loop 0 is written from the edit state. Type 0 = forward, 1 = alternating (ping-pong). (Type 2 = backward is read and preserved but not offered in the v0.1 UI.) **End is inclusive:** a loop of `L` frames from `s` is written as start `s`, end `s + L - 1`. This was confirmed against an existing file whose separate loop-range marker had length 384 for start 2532 and end 2915. Fraction 0, play count 0 (infinite).
- **Loop off:** write a `smpl` chunk with zero loops, so the root key and tune survive (this is how a one-shot carries its root key).
- **Additional loops** read from the file are written back unchanged after loop 0.
- **Sampler-specific data** following the loops is preserved.
- If the original file had no `smpl` chunk and the user changed nothing related to tuning or looping, do not add one.

### 6.6 Saving WAV files

v0.1 saves individual WAV files only. The save dialog follows `layout-v4-wav.html`, with the SFZ option shown disabled and labelled "coming later".

- **Destination:** by default a new folder next to the source (e.g. `<source folder>/limewave/`), keeping file names. **Overwrite originals** is a second option (decided: offered). Before replacing a file it copies the original to `<name>.wav.bak`; if that exists it uses `<name>.wav.1.bak`, `.2.bak` and so on, never overwriting an existing backup. The backup is made before the atomic rename, and if the backup fails the save for that file is aborted.
- **Trimming (Start/End markers):** the saved file contains only frames `start…end` of the rendered audio. All `smpl` loop offsets are shifted by `-start` so they still point at the same audio. The crossfade is rendered on the full original first (it may read audio before Start), then the trim is applied.
- **What is written:**
  - `fmt ` and `data` in the original sample format (same bit depth and rate; v0.1 does no format conversion). If writing to integer formats after a crossfade, convert with rounding and clipping; dither is not required.
  - `smpl` per §6.5.
  - `LIST/INFO` with `ISFT` set to `Limewave <version>` (replacing any existing value). All other INFO fields (e.g. `INAM`) are kept. If the file had no INFO list, one is created containing just `ISFT`.
  - `cue ` and `LIST/adtl` chunks are **not written** (the user does not want cue markers). Dropping them also avoids leaving stale loop markers that disagree with `smpl`. *My call: v0.1 drops these chunks entirely; the inspector warns "markers will be removed on save" when a file has them.*
  - All other chunks: copied through unchanged.
- **Crossfade is destructive:** when a crossfade length `N > 0` is set, the saved audio has the crossfade mixed in. For `i` in `0..N`, the sample at `loop_end - N + 1 + i` becomes `fade_out(i) * x[loop_end - N + 1 + i] + fade_in(i) * x[loop_start - N + i]`, where the fade curves are linear or equal-power and `fade_in` rises from 0 to 1 across the window. `N` is clamped so it never reaches before the start of the file (`loop_start - N >= 0`) or beyond the loop length. The loop points themselves don't move. *The builder may choose a different standard loop-crossfade formula if it is documented and tested, as long as playback through the seam is continuous.*
- **Trim after loop end (optional checkbox, off by default):** a shortcut that sets End to `loop_end + P` (default `P` = 16, clamped to the file length) so sampler interpolation still has data to read past the loop end. It's the same trim as dragging End there.
- **After saving**, the open document is rebased onto what was just written: the editor now works from the saved audio (crossfade already mixed in), the crossfade setting resets to 0 with a note "crossfade applied in saved file", and the next Save compares against and writes to that same destination. The original source file is untouched (unless overwriting). Undo history starts fresh after a save, because a trim shifts every frame position and the old history would point at the wrong audio. *My call.*
- **Re-crossfading:** a WAV doesn't record that a crossfade was applied (the user asked for `smpl`-only metadata), so reopening a saved file shows crossfade 0. That is accepted; adding another crossfade simply fades again. No private chunk or sidecar file. *My call.*
- **Only save files that changed** (since the last save, or since opening) is on by default in folder mode.
- **Save** (toolbar) re-saves using the last choices for that file without showing the dialog; **Save as…** always shows it.
- Writing is atomic: write to a temporary file in the destination folder, then rename over the target.

### 6.7 Audition

- Output through `cpal` on the default device, at the device's native rate. A sample at a different rate is resampled on the fly by the playback interpolation (below).
- **Play button:** plays from Start through the loop (looping until stopped) at the root key. A second mode plays only the loop region.
- **Keyboard notes:** two rows of the computer keyboard map to about two octaves (the common "Z row = lower octave, Q row = upper octave" layout), with octave up/down keys. Playing a note plays the sample transposed by `2^((note - root - fine/100)/12)`. Up to 8 voices; a short release fade on key-up so notes don't click off. Fixed velocity in v0.1.
- **MIDI input:** via `midir`, choosing a device in a small settings menu. Include it if it fits; otherwise it moves to the next milestone.
- **Interpolation:** at least linear, preferably 4-point (cubic Hermite) for clean transposition. Loop wrap uses the rendered (crossfaded) buffer so the audition sounds like the saved file.
- **Ping-pong loops** play forward then backward between the loop points.
- **Live edits:** loop points, crossfade and tuning can change while notes sound. The UI thread renders the new playback buffer (or the small crossfade region) and hands it to the audio thread without locking (e.g. an atomic swap of an `Arc` pointer or a lock-free queue). If a playing voice is beyond a new loop end, it wraps to the new loop start on its next sample. **Never allocate, lock or block on the audio thread.**
- The keyboard strip at the bottom highlights the root key and lights keys as they play; clicking a key plays it.
- Note naming: MIDI 60 = C4 by default (*my call*; a preference to show 60 as C3 can come later).

### 6.8 Undo, unsaved changes and closing

- Ctrl/Cmd+Z and Ctrl/Cmd+Shift+Z (and Ctrl+Y on Windows) undo/redo per file.
- Closing the app or opening a different folder with unsaved changes asks: Save all / Discard / Cancel.

### 6.9 Performance targets

- Opening a 60-second 48 kHz 24-bit stereo WAV and showing its waveform: under 1 second on a typical laptop.
- UI stays responsive at 60 fps while dragging markers during playback.
- Audio buffer of about 256–512 frames at 48 kHz without dropouts on a typical laptop.

## 7. Architecture (decided parts)

A Cargo workspace with at least two crates:

- **`limewave-core`** (library, no UI, no audio device code):
  - RIFF chunk reader/writer with `smpl` and `LIST/INFO` handling and unknown-chunk pass-through
  - sample decode/encode to/from `f32`
  - edit-state types and undo history
  - DSP: peak pyramid, zero-crossing search, crossfade render, trim
  - later: pitch detection, envelope analysis, classifier, loop finder, tail rendering, SFZ read/write
- **`limewave`** (the app binary): `eframe`/`egui` UI, `cpal` audio engine, `midir` MIDI input, file dialogs.

Keeping the core free of UI and device code keeps it unit-testable and leaves room for a CLI and a CLAP plugin later. Further splitting is the builder's choice.

Suggested crates (all permissive): `eframe`/`egui`, `cpal`, `midir`, `rfd` (native file dialogs), `hound` (optional). The builder may choose equivalents if they are permissively licensed.

## 8. Testing

- **Unit tests in `limewave-core`** using synthetic WAVs generated in the test (sine, decaying sine, noise; mono and stereo; 16-bit, 24-bit, float):
  - round-trip: read → write with no edits → byte-identical for files that already contain `smpl` and our `ISFT` (and only `ISFT`/`cue`-related differences otherwise)
  - unknown chunks preserved in order
  - `smpl` encoding: inclusive loop end; negative and positive fine tune; zero-loop `smpl` for one-shots; extra loops preserved
  - crossfade: a sine with a loop of exactly `k` periods plus crossfade has a continuous seam; crossfade clamping at file start
  - trimming: head trim shifts `smpl` loop offsets by `-start`; tail trim and "trim after loop end" padding; crossfade that reads audio before Start still works after the trim
  - overwrite: a `.bak` is created (numbered when one exists), and a failed backup aborts that file's save
  - `inst` / `acid` root note and tune kept in step when present
  - zero-crossing search
  - malformed inputs don't panic
- **Optional sampler check:** a tiny hand-check pack (a sine at a known pitch saved with fine tune +25 ¢ and −25 ¢, and a looped file) to load into whatever sampler the user has, confirming tuning and loop play as expected.
- **Manual test pass** on the local `samples/` files (not committed): open both, confirm the loop (2532–2915) and root key 60 are read, nudge the loop, save, reopen, and check the result.
- **CI:** build and run the tests on Windows, macOS and Linux (GitHub Actions or similar). *Builder's choice of service.*
- A dependency licence check (e.g. `cargo deny`) in CI to keep GPL-family crates out.

## 9. Roadmap after v0.1

*The order below was changed on 2026-10-20: v0.2, then tail rendering, then SFZ, then the rest. See the v0.2 design, §6.*

Each milestone gets its own short design pass. The DSP details for M2–M5 are already worked out in the local planning brief (`docs/LOOP_EDIT_HANDOFF.md`, git-ignored), which the builder should read when those milestones start.

1. **M1: SFZ instruments.** The document can be an `.sfz` instrument. Region list grouped like SFZ `<group>`s; key-range × velocity map along the bottom (click a box to select its region, drag to resize ranges, unmapped keys hatched, "auto-map from root keys", "fill gaps"); region mapping fields in the inspector; save `.sfz` plus its WAVs, with the option to also write `smpl` into each WAV. Loop modes gain SFZ's `one_shot`, `loop_continuous`, `loop_sustain`. Open existing `.sfz` files. Mockup: `layout-v2-sfz.html`. The save dialog's "SFZ" and "Both" options switch on.
2. **M2: Pitch detection.** YIN/pYIN to note + cents with confidence; "Detect" button and ½× / 2× octave fixes; parse root key from file names (e.g. `Piano_C3.wav`); flag files whose root key looks like a default.
3. **M3: Classifier.** Amplitude-envelope features → probabilities for complete one-shot / truncated / sustained / rhythmic, plus a suggested stable region. Badges in the file list; the class preselects sensible editor defaults; the user can override.
4. **M4: Auto loop finder.** Candidates from self-similarity in the stable region, refined at sample level (period-constrained for pitched material), scored by rendering the seam. Ranked list beside the seam view; candidates shown as markers; a "tight ↔ natural" length preference.
5. **M5: Tail rendering.** For truncated samples: decay-fitted loop extension plus noise residual first; then the quilted (patch-search) tail; partial resynthesis later. Rendered into the audio on save, like the crossfade.

Possible later: batch processing over a folder, a command-line tool, a CLAP plugin.

## 10. Decisions from the review round

- "Overwrite originals" is offered, with numbered `.bak` backups (§6.6).
- Start and End markers trim the saved file in v0.1 (§6.2, §6.6).

## 11. Calls I made (please check)

- Licence MIT OR Apache-2.0.
- `cue ` / `LIST/adtl` chunks are dropped on save, with a warning in the inspector.
- Default save folder `<source>/limewave/`; `.bak` copies when overwriting.
- MIDI 60 is named C4.
- `samples/` is git-ignored because its files are not ours to publish; tests use generated audio.
- The `docs/` folder stays git-ignored except `docs/superpowers/`, so this spec and later plans are committed while the reference material stays local.
- v0.1 edits only the first loop and preserves any others.
- MIDI keyboard input is "include if cheap" in v0.1.
- `inst` / `acid` chunks are kept in step with the root key and tune when present.
- The fine-tune sign follows the published RIFF definition; a real-sampler check is optional.
- After a save, the document continues from the saved file and undo history starts fresh; crossfade settings are not stored in the WAV.

## 12. Left to the builder

- Exact crate versions and any equivalent permissive crates.
- Internal module structure beyond the two-crate split.
- Zero-crossing search window, seam-view zoom range and default, nudge step sizes.
- Exact visuals within the stated theme and layout; panel sizes; keyboard shortcuts beyond those listed.
- Whether the reserved panel next to the seam view shows loop-length info or is hidden in v0.1.
- CI service and packaging/installers (not required for v0.1; a built binary per platform is enough).
