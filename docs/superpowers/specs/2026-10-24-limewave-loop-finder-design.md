# Limewave v0.8: auto loop finder

## Intent

Finding loop points by hand is the slowest part of looping a sample. You drag the markers, nudge, listen, look at the seam, and do it again. The loop finder does the hunting. It searches the sound for places where a jump from the loop end back to the loop start is hard to see in the seam view and hard to hear, then shows a short ranked list. The user previews candidates on the keyboard, looks at each one in the seam view, and keeps the one they like.

It follows the product rule from the first design: **analysis suggests, never decides.** Nothing changes until the user picks a candidate, and picking is one undo step.

It must work on the two kinds of material the reference files represent:

- **Sounds that truly repeat.** `81 Track81_008.wav` (44.1 kHz, stereo, 11.9 s) is periodic from about 1.5 s onward: the audio at `t` matches the audio at `t + 228,278` to within −40 dB. Its stored loop (297,106–525,383, 5.18 s) is one of several identical passes. The finder should find that exact length and a seam that is sample-perfect.
- **Textures that never repeat exactly.** `81 Track81_009.wav` (44.1 kHz, stereo, 5.0 s) decays for about half a second and then holds a moving texture. Its stored loop (160,577–222,039, 1.39 s) is a tight local splice. The waveforms on either side of its seam agree for about 8 samples and then drift apart (correlation 0.5 over 200 samples). The finder should find splices at least as good as that, and better where the sound allows it.

Both files are rips with the loop ending about 10 frames before the end of the file. The files are in `samples/` (third-party and git-ignored), so they are for tuning and the manual checklist, not for CI tests. Both are close to mono (L/R correlation 0.99999).

A throwaway Python prototype of the method below, run without the stored loop points, gave:

- **008:** every top candidate has length 228,278, with a sample-exact seam. With the loop end kept at 525,383, the best start is exactly 297,106.
- **009:** the top candidate is 46,200–127,487 (1.84 s), with a waveform match of 0.99 across the seam. With the loop end kept at 222,039, the best starts are 82,723 (3.16 s, 0.99) and 153,225 (1.56 s, 0.96).

The search took well under a second for either file in the prototype.

## Goals

### Where it lives

- A **Loop finder** panel sits beside the seam view, in the space the first design reserved for loop candidates.
  - The seam panel is split side by side: the seam view on the left and the finder on the right.
  - The split is resizable, and egui's layout memory keeps its width.
- It acts on the file in the editor. In instrument mode it acts on the selected region's sample, the same way the inspector's Loop section does.

### Searching

- A **Find loops** button starts a search. Nothing runs on its own when a file opens, so opening a folder stays fast.
- **What is searched:**
  - With a selection, both loop points are searched inside the selection.
  - Without one, they are searched inside the **steady part** of the sound. That is the stretch after the attack has settled and before any final fade or trailing silence (see Mechanics).
  - Either way, the range is limited to Start..End. A loop outside the trim would not be saved, and the marker rules would clamp it into a different loop from the one previewed.
  - The panel says which range it searched. For example: "Searched the steady part, 0.55–5.00 s" or "Searched the selection".
  - While the panel is open, the searched range is drawn faintly in the waveform.
- **Keep loop end** (a toggle, off by default, available while a loop is set): the current loop end stays put and only the start is searched. This suits rips whose loop end is already right at the end of the file.
- **Length limits:** a minimum and a maximum loop length, in seconds.
  - The defaults are a minimum of 0.1 s and no maximum (the whole range).
  - The fields refuse a minimum above the maximum.
- **Tight ↔ Natural:** a slider for the length preference, from the original roadmap.
  - Tight favours short loops. Natural favours long ones, which usually sound less mechanical on pads and textures.
  - It breaks ties between seams of similar quality. It never lifts a clearly worse seam above a clearly better one.
  - The default is the middle.
- **Loop direction:**
  - The search is for the loop mode the sample has. Off and Forward search for forward loops; Backward searches for backward loops (it scores the time-reversed audio).
  - With Ping-pong set, Find loops is disabled. A note says the finder makes forward and backward loops.
- **Background:** the search runs on a worker thread through `render_worker.rs`. The panel shows "Searching…" and a **Cancel** button, and the window never stalls.
- **Settings memory:** the length limits, Tight ↔ Natural and Keep loop end are global (not per file). They are remembered between runs through eframe storage, like the envelope settings.

### The candidates

- Up to **8 candidates**, best first.
  - They must be genuinely different (see Distinct results in Mechanics).
  - In a sound that repeats, the same seamless length shows up at many positions, and only the best one is listed.
  - A texture can list two good positions of a similar length, because they sound different.
- Each row shows:
  - its rank;
  - its **length** in seconds and frames, plus **cycles** (length ÷ period, to one decimal place) when the sound is tonal;
  - its loop start and loop end in frames;
  - a **quality** word: *Seamless*, *Good*, *Fair* or *Rough*.
  - Hovering the quality word shows the measures it came from, in plain words. For example: "Click: none · Tone change: slight · Waveform match: 99 %".
- **Markers:** while the panel has results, every candidate is drawn in the waveform and the overview as a faint bracket over its loop span. The selected candidate is drawn brighter.
- **Selecting a row previews the candidate.** Clicking a row, or using the up and down arrow keys while the list has focus, shows that loop:
  - the loop markers, the seam view and the inspector's loop fields show it;
  - notes sounding on the keyboard play it at once, as with a marker drag.
  - The undo history is not touched. Clicking the same row again, or pressing Escape, ends the preview and goes back to the current loop.
- **Use loop** (a button, and also a double-click or Enter on a row) applies the selected candidate:
  - one undo step sets the loop start and loop end;
  - if the loop was Off, it turns Forward on;
  - the loop mode is otherwise kept;
  - the crossfade is kept, clamped to what fits the new loop;
  - Undo brings back the previous loop exactly.
- **Stale results:** if the audio changes after a search, the list greys out and says "The sound has changed. Search again." Changes to the audio include an effect, a crop, or an undo or redo across one. Moving markers by hand (Start, End, or the loop points) does not make results stale, because the candidates still describe the same audio.
- **No usable result:**
  - Silence, or a range shorter than the minimum length, gives a short message, such as "Nothing to loop in this range" or "The range is shorter than the minimum loop length".
  - Noise and other hard material still gets candidates, labelled honestly as *Fair* or *Rough*. The finder never claims *Seamless* for a seam that is not.

### A bench for tuning

- A `loop_lab` example in `limewave-core`, next to `tail_lab`, runs the finder on a WAV from the command line. It prints the candidates with their measures and the time taken, and writes a listening file for each candidate into `target/loop-lab/<name>/`: one second of lead-in followed by three passes of the loop.
- `--keep-end <frame>`, `--range <first> <last>`, `--min`/`--max` and `--preference` mirror the panel.
- Its purpose is to tune the quality thresholds on real files such as the two references.

## Non-goals

- Ping-pong loops.
- Suggesting a crossfade length. The finder looks for splices that work with no crossfade, and the user's crossfade setting is kept.
- Searching on its own when a file is opened, and loop badges in the file list. These need the classifier, which is still to come.
- Batch finding across a folder.
- Pitch-locked looping for tuned instruments: forcing the length to a whole number of cycles, or retuning. Cycles are shown, and a good seam on tonal material lands near whole cycles anyway, but it is not enforced.
- Multiple loops per file. The `smpl` chunk has room for more, but Limewave edits loop 0.
- Changing the audio. The finder only places markers. "Make loop from spectrum…" remains the tool that rebuilds audio.

## Anti-goals

- A candidate ranked *Seamless* or *Good* that clicks, thumps or audibly changes tone at the seam.
- A list of near-duplicates: eight copies of the same loop shifted by a few samples.
- Changing the user's loop without an explicit Use loop, or a Use loop that takes more than one Undo to reverse.
- A window that stalls while the search runs.
- A search that gives different results for the same audio and settings.
- Any third-party product or hardware name in code, UI text, docs or commit messages.

## Mechanics (the builder's choice inside these lines)

### Core

- **Module:** a new `loop_finder` module in `limewave-core`, as pure functions on the channel slices, with unit tests, like `tail` and `spectrum_loop`. It reuses `dsp` where it fits: the FFT framing, `envelope`, and `pitch::character` for tonal material and the cycles column.
- **Shape of the API:** a suggestion.
  - `steady_range(audio, rate) -> Option<(usize, usize)>`
  - `find_loops(audio, rate, &FinderSettings, &progress) -> Result<Vec<LoopCandidate>, FinderError>`
  - `FinderSettings` carries:
    - the range;
    - an optional kept end;
    - the minimum and maximum length in frames;
    - the preference, from 0 (tight) to 1 (natural);
    - the direction (forward or backward);
    - the candidate count.
  - `LoopCandidate` carries `loop_start` and `loop_end` (inclusive, as everywhere else), the overall score, the quality and the separate measures behind it, and the period when the sound is tonal.
  - How cancelling and progress are reported (an atomic flag, a callback) is the builder's call, and should match how the worker is used for tails and spectrum loops.
- **Channels:** all channels count.
  - Correlations and distances are taken per channel and combined, as `pitch::character` does, so out-of-phase stereo does not cancel out.
  - A seam that is clean in one channel and bad in the other is bad.
- **Search range cap:** at most **60 s** of audio is searched. A longer steady part is searched over its last 60 s. A longer selection is refused with a message asking for a shorter one. This bounds the time and memory needed.

### Method

The method is a suggestion, and the prototype showed it works on both references:

1. **Steady part.**
   - Take the level envelope in steps of about 50 ms.
   - Its reference level is the median over the second half of the sound.
   - The steady part starts at the first point after which a short running median stays within about 3 dB of the reference.
   - It ends at the last point that is no more than about 6 dB below the reference, which drops any final fade or trailing silence.
   - On the references this gives about 0.2–11.9 s for 008 and about 0.55–5.0 s for 009.
   - A sound with no steady part (a decaying one-shot) falls back to the middle 60 % of the sound, and the panel says it found no steady part.
2. **Coarse search.**
   - Describe the range as short frames of band levels: for example, 2048-sample frames with a 512-sample hop and third-octave band energies in dB.
   - Compare every pair of frames that is a valid loop: the start frame against the frame just after the end, with the gap between them within the length limits.
   - Average each comparison over a few frames on either side of the seam, so that both what leads into the jump and what follows it must match.
   - Keep the best few dozen distinct pairs.
   - Work along the lag diagonals so memory grows with the frame count, not its square.
3. **Refine to the sample.**
   - For each coarse pair, keep the end and slide the start through about ±1 hop.
   - Maximise the normalised correlation of the audio around the end (before it, and after it where the file has audio there) with the audio around the start.
   - With Keep loop end set, the coarse pass already fixes the end, so this is the whole search: one FFT correlation of the context around the end against the range, as in the prototype.
4. **Score by rendering the seam.** Build the spliced audio (up to the loop end, then from the loop start) and measure it against the sound's own natural change, so the same thresholds work for a clean tone and for a noisy texture:
   - **Click:** the energy of the second difference across the join, divided by the typical second-difference energy of the material.
   - **Tone change:** the band-level difference between the frame before the join and the frame after it, divided by the sound's typical frame-to-frame change in the range. A value near 1 means the seam changes no more than the sound does by itself.
   - **Waveform match:** the normalised correlation of a few milliseconds of context on each side of the join.
   - The overall score combines the three measures and then the length preference.
   - At the middle of Tight ↔ Natural the preference is neutral.
   - **Whole multiples:** a candidate whose length is a whole multiple (±0.1 %) of another candidate's, with a seam no better than that one's, ranks directly below it, whatever the preference. It is the same repeat played twice. This is why 008 with Keep loop end gives 297,106 (one pass) above 68,828 (two passes). On 009, 82,723 ranks above 153,225 because its seam is better, not because it is longer.
   - The quality words are thresholds on the measures. The builder tunes them with `loop_lab` on the references and on synthetic material, so that:
     - a periodic signal's exact repeat is *Seamless*;
     - 008's best candidate is *Seamless*;
     - white noise is never better than *Fair*.
5. **Distinct results.** One rule applies to every sound:
   - Two candidates count as the same when their lengths differ by less than 1 % and either:
     - their starts differ by less than 50 ms, or
     - both are rated *Seamless*. Seamless repeats of the same length are interchangeable.
   - Of each such group, only the best is kept.
   - Two *Good* or *Fair* candidates of a similar length at different places therefore both stay, since in a texture they sound different.
6. **Backward loops:** run the same search on the time-reversed audio and map the points back.
   - Only the wrap is scored: playing backward, the jump from the loop start to the loop end.
   - The one-time turnaround when a note first reaches the loop end is not scored. It is heard once per note, belongs to how backward loops play in general, and the seam view does not show it either. A backward candidate's quality describes the wrap.
7. **Determinism:** there is no randomness. Ties are broken by the earlier start, then the shorter length.

### App

- The panel's state (settings, results, the selected row, the preview) lives in `UiState` and on the session. The builder decides which part goes where.
  - Results are tied to the file, the effects list they were searched on and the settings. A result for different audio is shown as stale and is never applied to the wrong audio.
  - `RenderKey` includes the history position, which every marker move changes, so on its own it is too strict for this.
- The preview reuses the session's existing `preview` edit, the one the views already draw through `shown_edit`. Audition follows it as it does during a marker drag.
- The search runs on the processed audio (after effects, before crossfade and trim) that the seam view and playback use, and frames are in that audio's coordinates, as the loop markers are.
- Use loop goes through `Session::apply` with `move_marker`-style clamping, so it obeys the usual marker constraints.

## Calls I made

You skipped the questions, so all of these are mine. Please check them.

- The finder lives in the panel beside the seam view, as the first design reserved, and not in a menu or a dialog.
- It runs only when asked. There is no search when a file opens.
- With no selection, the search range is the detected steady part. A selection overrides it.
- Selecting a row only previews. Use loop commits, as one undo step.
- Forward and backward loops are supported. Ping-pong is not.
- There are no crossfade suggestions. The user's crossfade is kept.
- Candidates are not forced onto zero crossings, and the toolbar's Snap does not affect them. A matched splice is click-free wherever it lands, and the seam view shows it.
- There are up to 8 candidates. Seamless candidates of the same length are collapsed into one, while texture candidates of a similar length at different places are kept.
- The search range is always limited to Start..End.
- A whole multiple of a candidate ranks directly below it. Otherwise the middle of Tight ↔ Natural is neutral.
- For backward loops only the wrap is scored, not the one-time turnaround at the loop end.
- Stale results can follow the session's existing `audio_version` counter, which changes with the audio and not with marker moves.
- The quality words are *Seamless*, *Good*, *Fair* and *Rough*.
- The search covers at most 60 s, and a longer selection is refused.
- The settings are global and remembered between runs.
- There is a `loop_lab` bench for tuning, like `tail_lab`.
- The version is v0.8.0.

## Done when

### Unit tests (synthetic, in `limewave-core`)

- **Exact repeat.**
  - Take a 3 s stereo signal made of a 0.7 s noise burst tiled end to end, with a 0.3 s fade-in.
  - The top candidate's length is exactly a multiple of the tile length, to the frame.
  - Its splice error is below −60 dB, and it is labelled *Seamless*.
  - With Keep loop end set, the end is unchanged and the start lands exactly one or more tile lengths earlier.
- **Tonal.**
  - Take a 220 Hz tone with 8 harmonics at 44,100 Hz, with a slow 2 Hz vibrato of ±5 cents.
  - The top candidate has no click, measured as a second-difference ratio below the *Good* threshold.
  - The cycles column reads within 0.1 of a whole number.
- **Noise.** White noise gets candidates, but none is better than *Fair*.
- **Limits.**
  - Every candidate lies inside the search range.
  - Every candidate's length lies within the minimum and maximum.
  - No two candidates are duplicates under the rule above.
- **Steady part.**
  - For a sound with a 0.5 s attack, 3 s of steady level and a 1 s fade, the steady part starts within 100 ms of the attack's end and ends within 100 ms of the fade's start.
  - A pure decay reports no steady part and falls back as described.
- **Backward.** The exact-repeat signal, reversed and searched as Backward, gives the matching backward loop.
- **Edges.**
  - Silence, an empty file and a range shorter than the minimum give the right `FinderError`.
  - A 61 s selection is refused.
- **Determinism.** Two runs on the same input give identical lists.

### App tests (egui_kittest)

- **Use loop and Undo.**
  - Find loops, then Use loop on the first candidate.
  - The loop markers move, a loop that was Off becomes Forward, and it took one undo step.
  - Undo restores the previous loop exactly.
- **Preview.**
  - Selecting a row moves the shown loop, and the undo history length is unchanged.
  - Escape goes back to the current loop.
- **Stale results.**
  - Applying an effect after a search greys out the list and disables Use loop.
  - Moving a marker by hand does not.
- **Ping-pong:** Find loops is disabled.

### Manual checklist (reference files from `samples/`, beside the v0.8 plan)

- **008, whole sound:**
  - the first candidate is 228,278 frames long, labelled *Seamless*;
  - three passes in `loop_lab` sound unbroken.
- **008, Keep loop end at 525,383:** the first candidate starts at exactly 297,106.
- **009, whole sound:**
  - the first candidate's waveform match and tone change are better than the stored loop's, measured with the same measures;
  - by ear, three passes have no click and no obvious lurch in the texture.
- **009, Keep loop end at 222,039:** the candidates include a start near 82,723 or 153,225, or better.
- **Speed:** on a release build, either reference finishes in under 1 s, and a 60 s range finishes in under 5 s on the maintainer's machine. The window stays responsive throughout, and Cancel stops the search at once.

### Housekeeping

- The README describes the panel in British English.
- The version is 0.8.0 in the workspace `Cargo.toml`, the `software_tag` test and `Cargo.lock`.
- `cargo fmt`, `clippy -D warnings`, `cargo test --workspace` and `cargo deny check licenses` are all clean.
