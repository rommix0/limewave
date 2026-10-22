# Limewave v0.6: sample-rate conversion

## Intent

Change a sample's rate while it keeps its pitch and length. This works in either direction:
- up, to bring a library ripped at an odd rate from an old hardware sampler to a standard rate;
- down, to give new material the rate and character of an old sampler.

The user types the rate as a whole number of hertz. It is not picked from a fixed list, so unusual rates are possible.

## Goals

- **Change sample rate…** goes in the **Effects** menu. It converts the whole file to a typed rate from 1,000 to 768,000 Hz.
- The audio is resampled with a high-quality band-limited (windowed-sinc) filter:
  - pitch and duration are unchanged;
  - downsampling removes what the new rate cannot hold, instead of folding it back as aliasing.
- The conversion is one undoable step in the effects chain, like a tail or a spectrum loop. Undo and redo restore the previous rate.
- Every frame position follows the conversion, scaled by `new / old` and rounded to the nearest frame. This covers:
  - Start, End, the loop points and the crossfade length;
  - the selection and the play-from point;
  - steps applied after it;
  - cue markers, on save.
- Once converted, the file uses the new rate everywhere:
  - saved files: the `fmt` rate and byte rate, and the `smpl` sample period;
  - playback;
  - the inspector's format line;
  - the time ruler, which reads in milliseconds;
  - the zero-crossing snap window;
  - the tail and spectrum-loop dialogs.
- Root key and fine tune are unchanged, because the pitch is unchanged.
- In an instrument, saving a converted sample scales the frame opcodes (`offset`, `end`, `loop_start`, `loop_end`) on every region that uses it, followers or not. This is the same rule as a trim. No tuning opcode changes, because samplers play each file at its own rate.

## Non-goals

- Changing the rate number without resampling, which changes pitch and speed.
- Converting a folder or a whole instrument in one go.
- A quality or filter choice.
- Bit-depth conversion.
- Typed rates in the **New…** dialog, which keeps its fixed list for now.

## Anti-goals

- Audible artefacts from the resampler on ordinary material:
  - more than about 0.1 dB of passband ripple below 0.9 × the lower Nyquist;
  - aliasing above about −90 dB;
  - clicks at the file edges.
- Blocking the window while a long file converts.
- Any third-party product name in code, UI text or docs.

## The dialog

- A modal dialog in the style of the other effect dialogs (`dialog_kit`).
- It shows the current rate and a **New rate** number field in Hz, accepting digits only.
- A **Common rates** dropdown fills the field with 8,000; 11,025; 16,000; 22,050; 32,000; 44,100; 48,000; 88,200 or 96,000 Hz. Any other whole number in range can be typed.
- Below the field it shows the length after conversion in frames and seconds. It also warns when the new rate is lower: "Content above N Hz will be removed".
- **Convert** is disabled when the field is empty, out of range, or equal to the current rate. The dialog then says why.
- Convert renders in the background, through `render_worker`, the way tails do. "Converting…" shows while it runs, and Cancel still works. When the render is done, the step is applied as one undo entry and the dialog closes.
- In instrument mode, Change sample rate… acts on the selected region's sample, like the other effects.

## Mechanics (the builder's choice inside these lines)

- **Core:**
  - a new `Op::Resample(Arc<Resample>)`, where `Resample { from: u32, to: u32, audio: Vec<Vec<f32>> }`;
  - `audio` holds the converted audio, rendered once from the step's input (the audio after the earlier steps);
  - replaying the step swaps it in, like `Op::Splice`;
  - `first..=last` is always the whole input.
- **Effective rate:** a pure function `rate_after(file_rate, effects)` returns the `to` of the last Resample step, or the file's own rate. Everything listed under Goals reads this instead of `doc.format.sample_rate`.
- **Frame mapping:**
  - `map_frame` and `map_range` scale through a Resample step: `round(f * to / from)`, clamped to the new length − 1, with the end-of-file position landing on the new length;
  - cue remapping and the generated-audio tint follow from that;
  - a helper builds the next `EditState` with Start, End, loop points and crossfade scaled.
- **Resampler:**
  - a new `dsp/resample.rs` holding a Kaiser-windowed sinc table with linear interpolation between phases;
  - the cutoff is at the lower of the two Nyquists, times a 0.95 guard band;
  - edges are handled by holding the first and last frames beyond the file, so audio that starts or ends away from zero gets no click;
  - the output length is `round(input * to / from)`;
  - it is deterministic, so the same input and rates always give the same audio.
- **Rendering:** it runs on a `render_worker` thread. Cancelling closes the dialog and lets the render finish unseen, as the tail dialog does.
- **No new dependency.**

## Calls I made (the user skipped questions)

- It's an effect step, not a format setting at save time, so it can be heard and undone before saving.
- It always converts the whole file. A selection is ignored, because a file has one rate.
- Rates range from 1,000 to 768,000 Hz in whole hertz, since WAV stores the rate as a whole number.
- Rounding rule for markers: the loop length can change by a frame. A seamless loop may need a nudge in the seam view after converting; the README says so.
- Converting twice resamples twice. There is no shortcut back to the original rate except Undo.
- Quality is fixed at one high setting, with no choice offered.
- The version is v0.6.0.

## Done when

- A 1 kHz sine at 44,100 Hz converted to 31,250 Hz, then to 13,000 Hz, then to 96,000 Hz still measures 1 kHz, within 0.1 dB of its original level.
- The 31,250 and 13,000 Hz rates stand in for unusual old-sampler rates.
- A 10 kHz tone converted to 13,000 Hz comes out silent, below −90 dB.
- Saved WAVs carry the new rate and reload with the same markers, loop and cues.
- The RSB test library's odd-rate samples convert to 44,100 Hz, with their instrument regions scaled, and still play in tune.
- Undo returns the old rate everywhere it is shown.
- `cargo test --workspace`, fmt, clippy and `cargo deny check licenses` are all clean.
