# Limewave v0.7: spectrogram view and click repair

## Intent

Two small additions for musicians who clean up and loop samples.

1. **A spectrogram view.** A Waveform | Spectrogram switch changes how the main view draws the sound. The spectrogram shows where notes, harmonics, hiss and clicks sit in pitch. It is an extra way to look at the same file: markers can be dragged and selections made on it, and the editing works as it does in the waveform.
2. **Repair click.** The user selects a short click and Limewave rebuilds that stretch of audio from the sound on either side. Typical clicks are a few milliseconds long: digital glitches in old hardware sampler rips, or a bad edit point in a recording. The user finds one in the waveform or the spectrogram, drags a tight selection over it and chooses **Repair click**.

## Goals

### Spectrogram view

- A **Waveform | Spectrogram** switch sits above the main view.
  - Switching changes only what is drawn.
  - Zoom, scroll, the selection, Start/End, the loop markers, the crossfade, the play-from point and the playback cursors are drawn over the spectrogram exactly where they are in the waveform. They behave the same: markers drag, selections are made by dragging, and clicks and shortcuts do the same things.
  - The time ruler stays.
- **Frequency axis:** musical, with equal height per octave.
  - It runs from about C1 (32.7 Hz) at the bottom up to the file's Nyquist frequency at the top. That is half the file's effective rate, `rate_after`, so a converted file shows its new range.
  - It is labelled with note names at each C (C1, C2, …), with octave grid lines.
- **Colour:** a dark background with brightness in lime on a dB scale. Silence is near black and the loudest content is bright lime.
- **Generated audio** (tails, spectrum loops, repaired clicks) gets a faint lime overlay, as in the waveform.
- **Stereo:** the channels are stacked, one lane each, as in the waveform.
- **Background rendering:** it is computed off the UI thread, and the waveform shows until it is ready. The window never stalls while it computes.
- **The overview strip and the seam view stay waveforms.**
- **The pencil:** Draw mode only works in the waveform. Turning Draw on while the spectrogram is showing switches to the waveform.
- **Settings:** the choice is global, not per file, and is remembered between runs through eframe storage, like the envelope setting.

### Repair click

- **Repair click** goes in the **Effects** menu. It acts on the current selection and is disabled without one. In instrument mode it acts on the selected region's sample, like the other effects.
- **Channels (the user's decision):**
  - In a mono file, Repair click is a single menu item.
  - In a stereo file, it opens a submenu: **Both channels**, **Left only** and **Right only**.
  - Channels the user does not pick stay exactly as they were, sample for sample.
  - The selection itself stays a frame range across all channels, as it is today.
- The selected frames are rebuilt from the audio on both sides:
  - the sound is continued forward from the left side and backward from the right side;
  - the two continuations are blended across the gap;
  - the result is a fill that matches its surroundings in pitch, tone and level.
- **Length:** the sample stays exactly the same length. Start, End, the loop points, cues, the selection and instrument regions do not move.
- **Undo:** the repair is one undoable step in the effects chain, and Undo brings the click back.
- **Tint:** the repaired frames are tinted as generated audio, like a tail or a spectrum loop.
- **Stereo:** each picked channel is rebuilt separately from its own audio.
- **Size limit:** selections up to **50 ms** are accepted, measured at the file's effective rate.
- **Refusals:** a longer selection is refused with a short message, and so is a selection that does not leave enough real audio on both sides (it touches or nearly touches the start or end of the file). A refused repair changes nothing.

## Non-goals

- Finding clicks automatically.
- Crackle or vinyl-noise removal.
- A spectral editor: painting or erasing in the spectrogram. The user has held this off for now.
- Spectrogram settings: window size, dB range, colour maps or a linear frequency axis.
- A spectrogram in the overview strip or the seam view.
- Per-file view choice.

## Anti-goals

- A repair that makes things worse on ordinary material: a new click at either edge of the fill, a level jump, or a dull or noisy patch where the sound was steady.
- A UI that stalls while the spectrogram computes or a repair runs.
- Markers or selections that land in a different place in the spectrogram than in the waveform.
- Any third-party product name in code, UI text or docs.

## Mechanics (the builder's choice inside these lines)

### Repair

- **Core:** a pure function in `limewave-core`, in a new `dsp` module, rebuilds the frames `first..=last` of each channel from the context around them. An `edit.rs` helper, alongside `redraw_samples`, wraps the result as one same-length `Op::Splice` step:
  - a new `SpliceKind::Repair`, or reuse of `Draw`, is the builder's call;
  - `generated` covers the whole repaired range, so it is tinted;
  - nothing moves through it, the same as a Draw splice;
  - the helper takes the channels to rebuild. The splice's audio for every other channel is a copy of the input frames, so those channels come out bit-identical.
- **Method:** a suggestion. Use autoregressive (linear-prediction) interpolation:
  - fit a predictor to the context on each side;
  - extrapolate forward and backward;
  - crossfade the two across the gap.
  - Least-squares AR interpolation (solving for the missing samples directly) is an equally good choice.
  - The context length and model order should scale with the gap, so a 50 ms gap on a sustained note still comes back tonal rather than as a decaying fade.
  - The method must be deterministic.
- **Context:** a selection needs real audio on both sides, at least about as long as the gap itself or some minimum, whichever the method needs. The exact rule is the builder's. If the context is too short, the repair is refused with a message saying the selection is too close to the start or end.
- **Rendering:** if a 50 ms repair at 96 kHz takes long enough to be felt, it runs on `render_worker` like a tail. Otherwise it is applied directly.
- **No new dependency:** `realfft` and `dsp/lsq.rs` are already there.

### Spectrogram

- A short-time FFT using `realfft`, already a dependency.
  - Window, hop and FFT size are the builder's, chosen so pitch detail around the middle of a keyboard is readable. A larger FFT for the low octaves is fine.
  - Columns map to screen pixels through the same `ViewTransform` as the waveform.
  - The dB floor is about 90 dB below full scale *(my call)*.
- Rows are resampled from linear FFT bins onto the octave axis.
- It is computed on a worker thread and cached per file and edit, so scrolling and zooming do not recompute what is already known. Recomputing for the visible range and zoom is fine.
- After an edit, the previous picture stays up until the new one is ready, if the length is unchanged. Otherwise the waveform shows until it is ready *(my call: avoids a flash on every edit)*.
- A new `ViewMode { Waveform, Spectrogram }` lives in the UI state, saved under its own storage key.

## Calls I made

- Repair click sits in the Effects menu, with the other effects that act on a selection.
- Each picked channel is repaired from its own audio.
- Repaired audio is tinted as generated audio. In the code, pencil redraws are not tinted, so the tint follows tails and spectrum loops rather than the pencil.
- Selections up to 50 ms are accepted. The longer end works best on steady sounds such as sustained notes, which is most of what gets looped. The README says so.
- The spectrogram's dB floor is about −90 dB, with no setting.
- The previous spectrogram picture stays up after a same-length edit until the new one is ready.
- The version is v0.7.0.

## Done when

- **Click in a sine, by measurement.** A 440 Hz sine at 44,100 Hz has a one-frame spike, or a 2 ms burst of noise. After the repair, the error against the clean sine in the repaired range is below −40 dB of the sine's level.
- **50 ms gap.** A 220 Hz sine with a 50 ms gap of zeros comes back with the gap's level within 1 dB of the sine's, with no step at either edge.
- **Refusals.**
  - A 60 ms selection is refused.
  - A selection touching frame 0 or the last frame is refused.
  - A refused repair leaves the file unchanged.
- **Undo.** Repair then Undo gives back the exact samples. The file length and every marker are unchanged by a repair.
- **Stereo.**
  - With Left only, a click in the left channel is repaired, and the right channel is bit-identical to before.
  - With Right only, the reverse holds.
  - With Both channels, both channels are rebuilt.
- **The view switch.**
  - Switching to Spectrogram and back keeps zoom, scroll, the selection and the markers.
  - Dragging a marker or making a selection in the Spectrogram view gives the same frames as in the waveform.
  - Draw switches back to the waveform.
  - The choice survives a restart.
- **The spectrogram picture.** A 440 Hz sine's brightest row lands on A4. A file converted to 22,050 Hz shows its top at about 11 kHz.
- `cargo test --workspace`, fmt, clippy and `cargo deny check licenses` are all clean.
