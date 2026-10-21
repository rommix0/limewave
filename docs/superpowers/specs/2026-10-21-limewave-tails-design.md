# Limewave tails and spectrum loops: design

Status: draft for approval · 2026-10-21

This document specifies version **0.3.0**. That version adds tail rendering and a related effect, **Make loop from spectrum**. It builds on the v0.1 design (`2026-10-19-limewave-design.md`) and the v0.2 design (`2026-10-20-limewave-v0.2-design.md`), which stay the reference for everything not changed here. Where they disagree, this document wins.

The background for the algorithms is the planning brief (`docs/LOOP_EDIT_HANDOFF.md`, local, §5 and §6). The settings below were checked with throwaway Python prototypes on the local samples (§9).

## 1. Intent

Many samples in old libraries and sound sets were cut off. Either the sound was still ringing or still holding when the file ends, or the file was shortened and given a quick baked-in fade. Limewave should be able to give such a sample a believable new ending, made from the sample's own sound, so it can be used again as a one-shot or as a looping instrument sample.

There are two different needs, so there are two modes:

- **Decay mode** finishes a sound that should die away (a piano note, a pluck, a drum, a reverb tail). It continues the sound past the cut point and lets it fade naturally to silence.
- **Sustain mode** lengthens a sound that should hold (a pad, an organ, a bowed or blown note). It continues the sound at a steady level for as long as you ask, and builds a **seamless loop** into the new audio so the sample can sustain for as long as a key is held. An optional release afterwards gives it a natural ending too.

Different sounds need different ways of continuing them, so both modes offer three **methods**:
- **Smooth**: continues the sound's spectrum.
- **Harmonic**: rebuilds the sound from its harmonics.
- **Granular**: stitches short pieces of the sample together.

Because the classifier and the loop finder come later, the user decides which files get a tail, where the cut is, and which mode and method to use. Limewave suggests a mode and a method but never acts on its own.

**Make loop from spectrum** is a separate effect, not part of tail generation. It averages the spectrum of a stretch of audio and turns it into a loop that repeats seamlessly. It can be steady and true to the original note, or detuned into a wide ensemble pad. It is useful for:
- building a sustain loop out of a note too short or too unsteady to loop by hand;
- turning any sound into a pad or a texture.

## 2. Goals, non-goals, anti-goals

### Goals

1. **Generate tail…** on the current file, with Decay and Sustain modes (§3) and three methods: Smooth, Harmonic and Granular (§4).
2. Every method builds the tail from the sample's own sound (its spectrum, its harmonics or its audio), so the tail keeps the sound's character.
3. The join at the cut point is click-free, and the level does not jump (§4.6).
4. Sustain mode writes a **ready-to-use loop** in the generated audio. The loop wraps with no click and needs no crossfade (§4.8).
5. **Make loop from spectrum…** as an effect on the selection, or on Start…End when nothing is selected. Its settings are FFT size, Detune, Pitch lock and Loop length (§5).
6. Audition before applying, in both dialogs, with an A/B against the original (§7).
7. Both are undoable like any effect, and both save as normal audio with correct `smpl`, `cue` and `fact` data (§6).
8. A new audition option, **Release plays after loop end**, so a held looped note can be released into the audio after the loop end, the way samplers play a sustain loop with a release (§7.3).
9. A **truncation test bench** that cuts real or synthetic samples short on purpose, renders a tail, and compares it with the real ending, by ear and by numbers (§8).

### Non-goals

- Granular or spectral methods driven by machine learning, and other methods beyond the three.
- Detecting cut-off samples automatically (that is the classifier milestone).
- Applying a tail or a spectrum loop to many files at once.
- Repairing damage inside a sample (clicks, dropouts, a cut in the middle).
- A pitch-detection feature for the root key. The local pitch estimates here only serve the methods; the pitch-detection milestone is still to come.

### Anti-goals

- **Tails that sound obviously machine-made**, for example:
  - a short grain repeating audibly;
  - a wave whose shape pulses or drifts;
  - a stereo image that collapses to the middle.
- **A click, a level jump or a tone change at the cut point**, or at the edges of a spectrum loop.
- **Results that change between runs.** The same file and settings must always give the same audio, so that undo, redo, replay and tests agree.
- **Freezing the app** while a tail or a loop renders.

## 3. Tail modes

Both modes replace everything after the **cut point** with new audio. The cut point is the **End marker**.
- **For a typical cut-off file**, End is already on the last frame, so the tail is simply added onto the end.
- **To test on a sample that is not cut off, or to remove a baked-in fade**, drag End earlier first. Everything after End is then thrown away and regenerated.

```
Decay:    [ original … cut ][ continued sound, fading to silence ]

Sustain:  [ original … cut ][ steady body ⟲ loop ⟲ ][ release (optional) ]
```

### 3.1 Decay mode

- The tail continues at the level the sound has at the cut and fades exponentially to silence.
- **Decay time** is shown and set as the time to fall 60 dB. Its default is fitted from the source region (§4.2). When the source is not decaying (a sustained sound cut off), the default is 1.0 s.
- **Darken** (0–100 %, default 0) makes the high frequencies die away faster than the lows, the way most real sounds end (§4.7).
- The tail ends when its level falls below −90 dBFS, or at 30 s, whichever comes first. The last 5 ms ramps to exactly zero.

### 3.2 Sustain mode

- **Body length** (default 2.0 s, range 0.1–30 s): steady audio at the level the sound has at the cut, with no drift.
- **Loop** (on by default): the body is built so it can loop seamlessly (§4.8).
  - On apply, loop 0 is set to the body, Forward, with crossfade 0.
  - The user can still move the loop or change it afterwards with the usual tools.
- **Release** (time to fall 60 dB; 0 = no release): a decay-mode tail rendered after the body, so the file also has a natural ending.
  - The default is the fitted decay time, or 0.5 s when the source is not decaying.
  - Samplers that play the audio after the loop end on key release use it. That is SFZ `loop_sustain`, which the SFZ milestone will write.
  - Limewave's own audition uses it when **Release plays after loop end** is on (§7.3).
  - With Release 0, the file ends at the loop end.
- **Darken** applies to the release only.

### 3.3 Methods

| Method | What it does | Strengths | Weaknesses |
|---|---|---|---|
| **Smooth** | Freezes the spectrum at the cut and lets each frequency fade at its own measured rate (§4.4). | No joins at all, the steadiest result. Highs fade faster without needing Darken. Works on tonal and noisy sounds. | Can sound static or slightly smeared on fast-changing sounds. |
| **Harmonic** | Rebuilds the sound from its harmonics as smooth tones, plus a quiet noise layer for breath and bow noise (§4.5). | The cleanest, most instrument-like result on pitched sounds. | Weak on noisy and bell-like sounds. Can sound a little "clean". |
| **Granular** | Stitches short pieces of the sample's own audio together (§4.3). | The most lively. Best for noisy, textured sounds such as reverb, breath, rain and cymbals. | Audible graininess on clean tonal sounds. |

**Default method**: Smooth for tonal sources, Granular for noisy ones, from the analysis in §4.2. The dialog marks this as "Suggested", and the user can pick any method in either mode.

There are no extra modes. A plain fade-out already exists as an effect, and a release ending is part of sustain mode.

## 4. How a tail is made

### 4.1 Source region

All methods analyse the **source region**, the audio just before the cut point.

- If there is a selection, the selection is the source region, clamped to end at the cut point.
- Otherwise the engine chooses it: the last 1 s before the cut, never including the first 20 % of the span between Start and the cut. This keeps the attack out, since a repeated attack sounds wrong.
- The region needs at least 30 ms, and at least four periods for tonal sounds. If it is shorter, the dialog says to select a longer source region and Apply is disabled.

### 4.2 Analysis (shared)

**Combining channels.** Channels are combined without summing their samples, because channels that are out of phase would partly cancel. Energies are summed per channel, and correlations are summed per channel.

- **Level envelope**: RMS in 10 ms frames, in dB.
- **Steady or decaying**: a straight-line fit to the dB envelope over the **whole** source region. A slope shallower than −3 dB/s counts as steady, which drives the mode suggestion.
- **Decay rate**: the same fit over only the **most recent** 40 % of the region (at least 50 ms), and never shallower than the whole-region slope. Decays usually speed up, and the tail should continue the latest rate.
- **Character**: normalized autocorrelation over periods of 40 Hz to 2 kHz.
  - The search starts after the autocorrelation first dips below zero. Otherwise a smooth, low sound reads as a very short period.
  - Among peaks within 90 % of the best, the shortest period wins. This avoids picking a multiple of the period.
  - A peak of 0.6 or higher means **tonal**, with that period. Anything lower means **noisy**.
  - The strongest repeat is kept even when the sound is noisy, and is used to size windows for low sounds (§4.3).
- **Level at the cut**: RMS of the last 20 ms before the cut. Every method starts its tail from this level.
  - A longer average reads too loud on a fast-falling sound. The prototype measured +5 dB at 50 ms.
- **Spectral frames** (Smooth and Harmonic):
  - Frame size: Hann-windowed frames of about 93 ms, rounded down to a power of two. The size is reduced so that at least 2.5 frames fit in the source region.
  - Placement: hop of a quarter frame, with the last frame ending exactly at the cut.
- **Per-frequency decay rate** (Smooth and Harmonic): a straight-line fit of each bin's dB level over the last 8 frames, smoothed across 9 neighbouring bins. In decay mode it is limited to between 0.5× and 4× the overall rate. When the sound is steady, every bin takes the default rate, and a user-set decay time scales all bins together.

### 4.3 Granular method (quilting)

This is an audio version of image quilting (brief §6). It continues the sound by stitching together short patches copied from the source region, choosing each patch so it joins smoothly onto what came before.

- **Flattening**: the source region is divided by its smoothed level envelope before patches are taken, with a floor so near-silence is not boosted. The smoothing window is 20 ms, or two periods if that is longer. Stitching then mixes texture only, and the target envelope is applied afterwards (§4.7).
- **Patch length**: tonal sounds use about 40 ms rounded to a whole number of periods, and at least four periods. Noisy sounds use 50 ms, or two of the strongest repeats if that is longer.
- **Overlap**: tonal sounds use whole periods adding up to at least 5 ms. Noisy sounds use 25 % of the patch.
- **Choosing the next patch**: each candidate start in the source is scored by normalized cross-correlation between its overlap and the end of the tail so far, summed over channels. A penalty applies for reusing a recently used source position. The engine then picks at random among the best three distinct candidates, using a fixed seed. This avoids audible repetition while staying deterministic.
- **Joining**: a linear crossfade for tonal sounds, because matched overlaps are correlated, and an equal-power crossfade for noisy ones.
- **Stereo**: one patch plan for all channels, copied channel by channel at the same offsets, so the stereo image is kept.

### 4.4 Smooth method (spectral freeze and decay)

1. **Freeze**:
   - **Magnitudes**: the mean of the last two spectral frames.
   - **Phase**: the phase of the last frame.
   - **Frequency per bin**: the true frequency comes from the phase change between the last two frames, using the standard phase-vocoder estimate.
2. **Peak locking**: tonal bins are the bins within 2 of a prominent spectral peak, meaning more than 12 dB above the local median and within 70 dB of the loudest peak. Every bin in a peak's region advances at that peak's frequency, so each harmonic stays one coherent tone instead of beating against itself.
3. **Harmonic locking**: when the source is tonal, each peak is snapped to an exact multiple of one fundamental. The fundamental is refined as the amplitude-weighted mean of the peak frequency divided by the harmonic number.
   - This keeps every harmonic in step, so the wave keeps its shape.
   - Without it, the prototype's sustain body pulsed and lost its peaks.
4. **Noise bins**: all other bins get a fresh random phase every frame, from a fixed seed. The same random offset is added in every channel, which keeps the stereo relationship.
5. **Synthesis**: overlap-add with Hann windows at the analysis hop, normalized by the summed squared window.
6. **Level**:
   - **Up to the cut**: each bin is carried from the frozen frame's centre up to the cut at its *measured* rate.
   - **After the cut**: each bin fades at its *target* rate (§4.2).
   - Starting the target decay at the cut keeps steady sounds from losing level at the join. The prototype lost about 3 dB without this.

### 4.5 Harmonic method (partial resynthesis)

1. **Partials**:
   - **Tonal source**: one partial per harmonic of the fundamental up to the Nyquist frequency. Each is found as the strongest bin within ±¼ fundamental of its expected place, kept if it is a local peak within 70 dB of the loudest. At most 60 partials are used, loudest first.
   - **Noisy source**: the prominent peaks from §4.4.
2. **Frequencies**: for tonal sources, exact multiples of the refined fundamental (§4.4), for the same reason.
3. **Amplitudes and phases**: a joint least-squares fit of cosine and sine pairs for every partial, per channel, over the last frame before the cut. The fit uses constant amplitude over the window, so it gives the window's average level. The level is then carried to the cut and decayed as in §4.4, using each partial's bin rate.
4. **Noise layer**: what the fit leaves over (the residual) is frozen as a random-phase spectrum and synthesized as in §4.4. It fades at the per-bin rates.
5. **Synthesis**: a sum of exponentially decaying cosines, plus the noise layer.

### 4.6 The join at the cut (all methods)

The original audio is crossfaded into the generated audio just *before* the cut, so there is no step at the cut itself:
- **Granular** crossfades over its patch overlap.
- **Smooth and Harmonic** crossfade linearly over the last quarter frame, because the generated audio is coherent with the original there.

**An existing loop is never touched.** The join overlap never reaches back to the loop end, so audio inside the loop, and the loop crossfade that reads it, stay exactly as they were.
- When the loop end is too close to the cut, the overlap is shortened to fit.
- It is never shorter than one period, or 2 ms for noisy sounds.
- If even that does not fit, Granular splices its first patch on at the cut at its best-matching offset. Smooth and Harmonic crossfade over whatever room there is, down to a single frame.

### 4.7 Envelope and Darken

- **Decay mode and the sustain release**: an exponential fade from the cut level (Granular), or per-frequency fades (Smooth, Harmonic).
- **Sustain body**: constant at the cut level.
- **Darken** makes the highs die faster than the lows:
  - **Smooth and Harmonic** raise each frequency's decay rate by up to (1 + 3 × Darken) at the Nyquist frequency, rising linearly with frequency.
  - **Granular** uses a one-pole low-pass whose cutoff falls as the tail decays. It is fully open at the cut and, at 100 %, reaches 500 Hz by the time the level is 60 dB down.
  - At 0 %, Darken does nothing.

### 4.8 Seamless loop in sustain mode

Every method makes the loop seamless by construction, not by crossfade.

**Granular**:
- The body's first patch comes from source position `o₁`.
- The last patch is taken from the source audio that ends exactly where `o₁` continues. Its length is chosen, in whole periods for tonal sounds, so the body reaches the requested length.
- A wrap from the loop end back to the loop start therefore continues the original audio exactly as it really continued.
- The join *into* the last patch is searched and crossfaded like every other join.
- If the source leaves no room for that last patch, the engine chooses a later `o₁`. If no `o₁` fits, it falls back to a loop crossfade of one overlap and says so in the dialog.

**Smooth and Harmonic**: the body is exactly periodic.
- The loop length L is rounded to a whole number of hops.
- Every frequency is rounded so it completes a whole number of cycles in L. For tonal sounds, the fundamental is rounded so that L holds a whole number of periods, and harmonics stay exact multiples of it.
  - With a 2 s loop, that moves the pitch by at most 0.25 Hz.
- Noise phases repeat every L, so the noise layer loops too.

**Loop points**:
- **Loop start**: the first frame after the cut.
- **Loop end** (inclusive): loop start + L − 1.

**The release** starts by playing exactly what a wrap would play, then fades. It never changes audio inside the loop:
- Granular continues from `o₁`.
- Smooth and Harmonic continue the periodic signal and start their decay at the loop end.

### 4.9 Determinism and replay

Rendering happens in two steps, like Normalize resolving to a plain gain in v0.2.

**1. Plan.** Rendering first produces a **tail plan** from the settings and the analysis. The plan holds:
- the cut point, the source region and the method;
- the fitted values and the seed;
- whatever replay needs in order to skip any search:
  - Granular: its patch list, each patch a source offset, a length and an overlap;
  - Smooth: its frozen bins (magnitude, phase, frequency and rate per bin and channel);
  - Harmonic: its partials (frequency, amplitude, phase and rate) and its residual spectrum;
- the loop points.

**2. Replay.** Replay applies the plan and is cheap. The plan, not the settings, is what the edit history stores.

The same settings on the same audio always give the same plan.

## 5. Make loop from spectrum (effect)

**Effects → Make loop from spectrum…** works on the selection, or on Start…End when nothing is selected. That range is the **source range**. The effect replaces the source range with a loop built from the source range's average spectrum, and sets the loop markers on the result.

This code is separate from tail generation. It shares only general building blocks (the STFT, the windows, the pitch estimate from §4.2), not the tail engine.

### 5.1 How the loop is made

1. **Average spectrum**: the Welch average of the power spectrum over the source range. That is Hann frames of the chosen **FFT size**, hop of a quarter frame, power averaged per bin and per channel.
2. **Pitch**: estimated as in §4.2, with a lower threshold of 0.45 because the whole range is averaged. It is then refined from the averaged spectrum, using parabolic interpolation on the first eight harmonics, weighted by their strength.
3. **Building the loop**. The loop is one inverse FFT of length L, which makes it exactly periodic by construction. How the spectrum is filled depends on Pitch lock and Detune:
   - **Pitch lock off** ("Plain"): the averaged spectrum is interpolated onto the loop's L-point grid and given random phases. This is for unpitched material and textures.
   - **Pitch lock on, Detune 0** ("Steady"): each harmonic becomes one exact tone, a whole number of cycles per loop.
     - The fundamental is rounded so that L holds a whole number of periods.
     - **Amplitudes**: the RMS average of least-squares harmonic fits over consecutive windows across the source range. Each window is four periods or 50 ms, whichever is longer.
       - The prototype read amplitudes off the averaged spectrum's peak bins instead. That under-read them by about 1.5 dB, because harmonics fall between bins.
     - **Phases**: whatever the entry transition arrives at (§5.3), so the loop continues the original's wave shape exactly where it takes over. Any phase per harmonic still loops exactly, so this choice is free.
   - **Pitch lock on, Detune > 0** ("Ensemble"):
     - Each harmonic is spread into a Gaussian whose width is Detune in cents at that harmonic's frequency, so higher harmonics spread wider in hertz.
     - Each harmonic keeps its energy, not its peak.
     - Random phases are used.
     - A quiet floor taken from the averaged spectrum, at −60 dB, keeps the source's noise.
     - The result is a chorus-like, moving pad.
4. **Channels**: per-channel magnitudes, with the same random phases in every channel.
5. **Level**: the loop's RMS matches the source range's. If its peak would exceed −0.2 dBFS, it is scaled down to fit.

### 5.2 Settings

| Setting | Range | Default | Notes |
|---|---|---|---|
| **FFT size** | 256 … 65536, powers of two | auto: the largest power of two ≤ the source length ÷ 1.5 and ≤ ~186 ms | The dialog shows the bin width in Hz. Larger sizes give sharper harmonics but average fewer frames. It warns when fewer than 3 frames fit. |
| **Pitch lock** | on / off | on when a pitch is found | Shows the pitch found, for example "97.3 Hz". It is greyed out when no pitch is found, and then Plain is used. |
| **Detune** | 0–50 cents | 0 | 0 gives Steady, more gives Ensemble. Disabled when Pitch lock is off. |
| **Loop length** | 0.1–30 s | 3.0 s | The dialog warns when the loop is too short to resolve the detune, that is when its bin width, sample rate ÷ L, is wider than the detune spread at the fundamental. |
| **Transition** | 10–1000 ms | 100 ms | The length of the entry and exit transitions (§5.3). It is limited to half the source range. The exit is also limited to the audio after the source range. |
| **New variation** | button | n/a | Picks a new seed for the random phases. The seed is stored in the effect, so replay is unchanged. |

### 5.3 Placing the loop in the file

The loop takes the place of the source range. The original sound turns into it smoothly, and turns back into the original after it. Neither transition ever changes a frame of the loop itself, so the wrap stays exact.

```
before:  [ audio before ][ source range a … b              ][ audio after b …           ]
after:   [ audio before ][ entry T ][ ⟲ loop, length L ⟲ ][ exit T ][ audio after b + T … ]
                         ^ a        ^ loop start           ^ loop end + 1
```

- **Entry transition**: the first T frames of the source range, `a … a + T`, become the transition. The original plays on there and gradually turns into the loop, which starts at `a + T`. At the transition's first frame the output equals the original, so there is no step where the selection begins.
- **Exit transition**: the T frames after the loop end take the place of the original's first T frames after the source range, `b … b + T`. They turn the loop's own continuation back into the original, and from `b + T` the output is the original audio again. You hear this when a released note plays past the loop end (§7.3), or in any player that ignores loops.
- **Length**: the file's length changes by T + L − (b − a).
- Loop 0 is set to exactly the loop, Forward, with crossfade 0.

**How each flavour transitions:**

- **Steady: a harmonic glide.**
  - **Entry.**
    - A least-squares fit of the harmonics at the start of the transition gives the original's harmonic amplitudes and phases there.
    - Across the transition, each harmonic's amplitude moves smoothly, on a raised-cosine curve, from the original's value to the loop's. The fundamental glides from the original's pitch to the loop's rounded pitch, and the phases follow that glide.
    - Whatever the harmonic fit does not explain (the residual: noise, and any changes inside the window) fades out over the same curve.
    - The loop's phases are the ones the glide arrives at, so the join into the loop is continuous in amplitude, frequency and phase.
  - **Exit.**
    - The same glide runs in reverse, from the loop into a harmonic fit of the original at `b`, while the original's residual fades back in.
    - The loop's phases are already fixed, so each harmonic also gets a phase correction, the shortest way round, spread over the transition. That makes it land exactly on the original's phase at `b + T`.
    - Longer transitions make this correction gentler.
  - With less than one fit window of audio after the source range, the exit falls back to the Plain crossfade below.
- **Ensemble and Plain: an aligned crossfade.**
  - The loop has random phases, so harmonics cannot be glided one by one. Because the loop is periodic, any rotation of it is an equally valid loop.
  - The engine rotates it to the offset whose run-in best matches the original over the entry transition, found by circular cross-correlation summed over channels. It then crossfades with equal power.
  - The exit is an equal-power crossfade from the loop's continuation into the original.
- **Typical uses:**
  - Select the steady body after the attack to get "attack, then a smooth turn into the spectrum loop, then the original release".
  - Select everything to make a standalone pad loop.
- **Pitch**: it moves only as far as the rounding in §5.1 needs, at most half of one bin, sample rate ÷ L. That is 0.17 Hz for a 3 s loop. The root key and fine tune are unchanged.

## 6. Edit model and saving

- **Both features are applied effects** in the v0.2 effects list (`effects` in the edit state). They are replayed in order like the others and undone by popping them.
  - They are the **first effects that change the frame count**.
  - Effects recorded after them use frame numbers on the new length, which replay order already guarantees.
  - The spectrum loop's stored step holds its settings, its seed and the resolved pitch and FFT size, and replay recomputes the analysis. The tail stores its plan (§4.9).
- **On apply of a tail**:
  - frames after the cut point are replaced by the tail;
  - **End** moves to the new last frame, and Start is unchanged;
  - in sustain mode the loop is set as in §3.2;
  - in decay mode an existing loop is kept as it is. It always ends at or before the cut, because End never moves inside the loop. This gives the common "sustain loop plus natural release" result.
- **On apply of a spectrum loop**:
  - the source range is replaced as in §5.3;
  - the loop is set;
  - markers before the source range stay where they are;
  - markers after it shift by the change in length;
  - a marker inside it moves to the start of the loop, except End.
  - When the source range was Start…End, End moves to the end of the exit transition, or to the loop end when there is no audio after it.
- **The generated frames are tinted** lightly in the waveform and the overview strip, so it is clear which audio is new. The tint follows the effect, so undo removes it.
- **Saving.** Saving follows v0.2 §4.8 with the frame count changing.
  - `fact` is updated.
  - **Cues and ranges**:
    - Cues inside replaced audio are removed, like cues outside Start…End in a v0.2 trim: everything after the cut for a tail, and the source range for a spectrum loop.
    - Cues after a spectrum loop shift by the change in length.
    - An `ltxt` range that crosses a replaced boundary is shortened to end at it.
    - An end-of-file cue (at the old frame count) moves to the new end of the file.
  - Kept `smpl` loops other than loop 0 that touch replaced audio are dropped, and the save dialog lists them.
  - `inst` and `acid` are untouched.
- **The length is no longer fixed.** v0.2 deliberately kept the frame count fixed, and the code relies on that. These places use the original frame count and must change to use the **length after effects**, which comes from the effects list (each length-changing step records its output length):
  - clamping of markers and effect ranges (`EditState::clamped` and its callers);
  - the history;
  - the save path: in-place patching of the original data bytes, changed-range detection, and the "unchanged" checks;
  - frame mapping for cues and extra `smpl` loops (`map_frame`).

  This is a large share of the milestone's work. A file with no tail and no spectrum loop must behave and save exactly as in v0.2.
- **Rendering runs on a worker thread.** The dialogs show that they are working, and the main window stays responsive. With 1 s of source and a 30 s tail, or a 30 s spectrum loop, the result should be ready in under a second on an ordinary machine.

## 7. User interface

### 7.1 Generate tail dialog

**Effects → Generate tail…** opens a dialog for the current file:

- **Mode**: Decay / Sustain, with a "Suggested" tag on the one the analysis points to (decaying → Decay, steady → Sustain).
- **Method**: Smooth / Harmonic / Granular, with a "Suggested" tag (tonal → Smooth, noisy → Granular).
- **Analysis line**:
  - the source region (from the selection or chosen automatically);
  - the character, either "tonal, 96.7 Hz" or "noisy";
  - the fitted decay, or "steady".
- **Settings** for the chosen mode (§3). Changing any setting re-plans after a 200 ms pause.
- **A small preview** of the last second before the cut, then the tail, with the cut marked. In sustain mode the loop is drawn.
- **Buttons**:
  - **Preview** plays the result from 1 s before the cut.
    - In sustain mode it holds on the loop until Stop, then leaves the loop and plays the generated release.
    - The preview uses the session envelope's attack but not its release, so the release you hear is the generated one.
  - **Play original** plays the untouched audio from the same point, for A/B.
  - **Apply** and **Cancel**.
- The dialog warns, without blocking, when:
  - the Granular seamless loop had to fall back to a crossfade;
  - the join overlap had to be shortened to stay clear of the loop;
  - the source region is short;
  - Harmonic or Smooth is chosen for a source the analysis calls noisy.

### 7.2 Make loop from spectrum dialog

**Effects → Make loop from spectrum…**:

- **Header line**: the source range (the selection or Start…End) and the pitch found.
- **Settings**: those in §5.2. Changing a setting re-renders after a 200 ms pause.
- **A small preview** of the loop's waveform, with its averaged spectrum drawn underneath. The source's spectrum is drawn faintly behind it, for comparison.
- **Buttons**:
  - **Preview** plays the loop, repeating, until Stop.
  - **Play original** plays the source range.
  - **Apply** and **Cancel**.

### 7.3 Release plays after loop end

A new audition setting, next to the envelope settings: **Release plays after loop end**. It is off by default, and remembered between runs like the envelope.

- **Off**: today's behaviour. A released note fades out inside the loop through the envelope's release.
- **On**: when a looped note is released, the voice stops wrapping and plays on through the loop end into the rest of the audio. The envelope's release still applies from that moment, as a sampler with a sustain loop does. A long envelope release lets the whole generated release be heard.

This applies to keys, MIDI and Play, and to every looped file, not only tailed ones. It is a session setting, not saved into WAVs. The SFZ milestone will write it as `loop_mode=loop_sustain`.

## 8. Testing

### 8.1 Automated tests for tails (synthetic signals, in `limewave-core`)

The test signals are:
- a decaying harmonic tone with a known decay time and a little noise;
- a steady harmonic tone with a sharp, pulse-like wave;
- steady noise;
- a stereo tone with a phase offset between channels.

Each is cut short on purpose, then tested with **every method**:

- **Decay fit**: the fitted time to −60 dB is within 15 % of the true value.
- **Join**: the largest sample-to-sample step within 2 ms of the cut is no more than 1.5 × the 99th percentile step inside the source region. The same test applies at the sustain loop wrap.
- **Level**: the RMS of the first 50 ms after the cut is within 1.5 dB of the level at the cut, allowing for the decay the tail is meant to have over those 50 ms. The sustain body's 50 ms RMS stays within 1.5 dB of the target throughout. For Smooth, it stays within 1 dB.
- **Wave shape** (Smooth and Harmonic, steady tone):
  - consecutive periods of the sustain body correlate at 0.98 or higher;
  - the body's peak level is within 3 dB of the source's.
- **Against the real ending**: for the decaying tone, the tail's 10 ms dB envelope is within 3 dB on average of the uncut signal's real ending, until both fall below −60 dBFS.
- **Stereo**: the correlation between the two channels in the tail is within 0.1 of the source region's.
- **Determinism**: rendering twice gives identical samples.
- **Edit model**:
  - apply, then undo, then redo works, and End moves;
  - effects applied after a tail keep their full range;
  - in decay mode, an existing loop and its crossfaded audio are unchanged, including when the loop end sits right at the cut;
  - cues after the cut are removed, an `ltxt` range that crosses the cut is shortened, and the end-of-file cue moves to the new end;
  - a save and reload round trip has the right frame count, `fact` value and `smpl` loop;
  - a file with no tail still saves byte-identical to v0.2.
- **Audition**: with Release plays after loop end on, a released looped voice passes the loop end and plays the audio after it. With it off, behaviour is unchanged.

### 8.2 Automated tests for spectrum loops

- **Periodic**: the loop rendered twice end to end has identical halves. The step across the wrap is no larger than the largest step inside the loop.
- **Pitch**: for a steady harmonic test tone, the Steady loop's pitch is within 5 cents of the source's.
- **Steady**:
  - the 50 ms RMS stays within 1 dB;
  - consecutive periods correlate at 0.99 or higher;
  - the peak level is within 3 dB of the source's.
- **Plain**: the loop's spectrum, smoothed to third-octave bands, is within 2 dB of the source's average.
- **Ensemble**: harmonic peaks widen as Detune grows. The −6 dB width at 30 cents is at least twice the width at 10 cents.
- **Transitions** (steady harmonic tone, steady noise, and a stereo tone, each with audio before and after the selection):
  - the loop's frames equal the generated loop exactly, so the transitions do not touch them;
  - no step at the entry start, the loop start, the loop end or the exit end is larger than 1.5 × the 99th percentile step inside the source range;
  - from `b + T` on, the output equals the original sample for sample;
  - for Steady, the first entry frame is within 1 % of full scale of the original;
  - for Steady, the fundamental's frequency moves smoothly through the entry, with no frame-to-frame jump above 1 Hz;
  - markers and cues after the range shift by the change in length.
- **Determinism and variation**: the same settings and seed give identical audio, and **New variation** gives different audio.
- **Edit model**: undo and redo work, the save and reload round trip is correct, and effects after it keep their full range.

### 8.3 Truncation test bench (`tail_lab`)

A command-line example program in `limewave-core` for listening tests on real samples:

```
cargo run --release -p limewave-core --example tail_lab -- <file.wav> --cut <seconds> --mode decay|sustain --method smooth|harmonic|granular|all [settings…]
```

It writes into `target/tail-lab/<name>/`:
- `original.wav`
- `cut.wav`: the file truncated at the cut
- one `tail_<method>.wav` per method: the cut file plus the tail, written through the normal save path, so the sustain loop is in its `smpl` chunk and the file opens in Limewave
- a `heard` version of each sustain file, with the loop played three extra times

It prints the join, level, wave-shape and envelope numbers from §8.1. Wherever there is a real ending after the cut, it prints the comparison with that ending too.

**In the app**, the same check is to drag End earlier and generate a tail. Then use Undo/Redo, or **Play original** in the dialog, to compare with the real ending.

### 8.4 Manual checks with the local samples

The `samples/` folder is git-ignored, so these are listening checks, recorded in the release checklist:

- **`0060_020274_0ACB.wav`** (11.025 kHz mono, 0.445 s): a steady tone, then a baked-in fade over the last ~70 ms. Cut at 0.33 s, before the fade.
  - All three methods in both modes. The loop must hold without a click or a pulse, and the release must sound natural.
  - Spectrum loop over 0.02–0.37 s at Detune 0, 10 and 30, and at two FFT sizes.
- **`oneshot.wav`** (a 44.1 kHz drum-like one-shot, about 55 Hz and falling): cut at 0.15 s and at 0.25 s, decay mode, all methods. This is the known hard case (§9), so judge it by ear.
- **`loop.wav`**: a spectrum loop over the whole file at Detune 0. The pitch should match its existing loop points (about 57.4 Hz).
- **A stereo sample** of your choice: check that the stereo width is kept by every method and by the spectrum loop.

## 9. What the prototypes showed

Throwaway Python prototypes on the local samples, 2026-10-21. The numbers are the same measurements as §8. In the tail table, a positive "Level step" is a drop below the cut level; the spectrum-loop table shows the movement in level.

**Tails on `0060_020274_0ACB.wav`, cut at 0.33 s:**

| | Join click (≤ 1.5 passes) | Level step | Loop wrap | Body steadiness |
|---|---|---|---|---|
| Granular | 0.46 | 1.4 dB | 0.46 | ±1 dB flutter (the "grain") |
| Smooth | 0.81 | 0.8 dB | 0.84 | 0.5 dB |
| Harmonic | 0.54 | 1.9 dB | 0.57 | 1.8 dB slow sway (the noise layer) |

**Spectrum loops:**

| | Pitch | Level movement |
|---|---|---|
| Steady, 0060 | 97.3 Hz, 56 harmonics | 0.4 dB |
| Steady, `loop.wav` | 58 Hz | 0.5 dB |
| Plain or Ensemble | n/a | 5–9 dB (the intended swirl) |

Every loop wrapped seamlessly.

**Spectrum loop on a selection, with transitions:**
- On 0060 the selection was 0.10–0.30 s, with a 100 ms transition and a 2 s loop. On `loop.wav` it was 0.03–0.12 s, with a 40 ms transition.
- Every join measurement fell between 0.08 and 1.32, where 1.5 passes. That covers the entry start, the loop start, the wrap, the loop end and the exit end, for Steady, Ensemble and Plain.
- Steady glided visibly from the original's spiky wave into the loop, and back into the original's fade on exit.

**Fixes the prototypes found, now in this design:**
- the level at the cut comes from the last 20 ms (§4.2);
- envelope smoothing is short (§4.3);
- steady or decaying is judged on the whole region, and the rate on the recent part (§4.2);
- the autocorrelation search skips its main lobe (§4.2);
- crossfades between matched tonal patches are linear (§4.3);
- the release starts with the wrap's own audio (§4.8);
- peaks and harmonics are locked (§4.4, §4.5);
- the target decay starts at the cut (§4.4);
- Steady takes its phases from a least-squares fit (§5.1).

**Known weak case.** A low drum whose pitch falls (`oneshot.wav`) is not detected as tonal by any method. The tails fade more slowly than the real ending, because no fit on the audio before the cut can know that the decay speeds up. The user sets the decay time by ear.

## 10. Calls I made (please check)

- **Tail modes.** There are two modes and three methods: Smooth (suggested for tonal sources), Harmonic and Granular (suggested for noisy ones). Any method can be used in either mode.
- **Cut point and replacement.** The cut point is the End marker, and everything after it is replaced. This also lets you remove a baked-in fade and simulate a cut-off. The source region is the selection, or else is chosen automatically.
- **Storage.** A tail is stored as a resolved plan, so replay never searches. A spectrum loop is stored as settings plus seed, and replay recomputes it, since the computation is cheap and deterministic. To change either, undo it and apply again.
- **Seamless loops by construction.** Sustain loops and spectrum loops are seamless because of how they are built. They need no loop crossfade, and are set as loop 0, Forward.
- **Pitch rounding.** Smooth and Harmonic sustain loops, and Steady spectrum loops, round the pitch by at most half a bin of the loop length. The root key and fine tune are not changed.
- **Tail defaults.**
  - Sustain body: 2.0 s, with the release on and its time taken from the fitted decay.
  - Decay tail: runs to −90 dBFS, or 30 s at most.
  - Darken: defaults to 0.
- **Spectrum loop defaults.**
  - Loop length: 3.0 s.
  - Detune: 0 (Steady).
  - Pitch lock: on when a pitch is found.
  - FFT size: automatic.
  - Transitions: 100 ms in and out.
- **Spectrum loop placement.** The spectrum loop replaces the source range and sets the loop. The audio before and after the range is kept, joined by an entry transition inside the start of the range and an exit transition over the start of the audio after it. Steady glides harmonic by harmonic, and its loop phases come from that glide. Ensemble and Plain rotate the loop for the best match, then crossfade.
- **Markers.** On applying a tail, End moves to the new end of the file. In decay mode an existing loop is kept, and the join never touches audio inside it.
- **Cues.** Cues in replaced audio are removed, like a trim. Later cues shift, and the end-of-file cue follows the new end.
- **Stereo analysis.** Channels are combined by energy and correlation, not by summing samples.
- **New audition setting, off by default.** "Release plays after loop end" is a session setting for all looped files, and the envelope release still applies. The tail dialog's preview always plays past the loop end and skips the envelope release.
- **Generated audio is marked.** It is tinted in the waveform, so it is clear which audio is synthetic.
- **The test bench** is a `cargo` example program. It is not a feature in the app.
- **No movement setting for now.** There is no "movement" or "life" setting for Smooth. Granular, and Ensemble in the spectrum loop, are the lively options.
- **The version** becomes 0.3.0, so `ISFT` reads `Limewave 0.3.0`.

## 11. Left to the builder

- The exact patch-search scoring weights, the reuse penalty and the stride of the search, within the limits in §4.3.
- The Darken curve for Granular.
- The module layout inside `limewave-core`, with tails and spectrum loops in separate modules. The envelope, decay-fit, pitch and STFT code should be shared and easy to reuse, because the classifier and loop finder will need it.
- How the dialogs' previews are drawn, and where the tint colour comes from in the theme.
- How the processed-audio cache handles steps that change the length.
- The exact off-by-one at the Granular loop wrap (the last patch must end where the loop-start frame's own source audio continues, allowing for the first overlap).
- The FFT implementation, which must have a permissive licence and pass `cargo deny`.
