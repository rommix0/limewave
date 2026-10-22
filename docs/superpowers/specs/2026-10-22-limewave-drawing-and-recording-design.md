# Limewave drawing, new samples and recording: design

Status: draft for approval · 2026-10-22

This document specifies version **0.4.0**. It adds three features:
- a **drawing mode** for drawing a waveform at any zoom;
- **new blank samples**;
- **recording** from an input device.

It builds on the v0.1, v0.2 and v0.3 designs (`2026-10-19-limewave-design.md`, `2026-10-20-limewave-v0.2-design.md`, `2026-10-21-limewave-tails-design.md`), which stay the reference for everything not changed here. Where they disagree, this document wins.

It also builds on the sample pencil, which is already on master (commit d893bbb):
- When zoomed in until samples show as dots (below 1 frame per pixel), dragging a dot moves that sample.
- Dragging sideways redraws the samples passed, on a straight line between pointer positions.
- Shift moves a tenth as far.
- Values stop at full scale.
- Each stroke is one undoable `Splice` step of kind `Draw` that keeps the length (`limewave_core::edit::redraw_samples`).

## 1. Intent

Limewave is a sample editor for musicians. The pencil already fixes single clicks and spikes by hand. This milestone turns it into a way of **making** sounds, not just repairing them:
- **Draw a waveform** straight onto a file at any zoom. What you draw is exactly what you hear: the shape becomes the sample values. The main use is building single-cycle and short looping waveforms, which are drawn on a short stretch and looped.
- **Start from nothing.** A new blank file gives an empty canvas to draw on, sized in frames, milliseconds or one cycle of a note.
- **Record a sound** from an audio interface or microphone straight into a new file. You can then trim it, loop it, add a tail to it and save it like any opened file.

All three create or change audio inside the existing edit model. Undo, saving, markers and effects keep working the way musicians already know from earlier versions.

## 2. Goals, non-goals, anti-goals

### Goals
- A **Draw** toggle (pencil) on the toolbar. While it is on, dragging on a channel lane draws at any zoom.
- **File → New…** creates a silent file from a small dialog and opens it, ready to draw on.
- **Record** opens a recording panel:
  - choose an input device and channels;
  - watch the level;
  - record a take into a new file.
- New and recorded files are **untitled until saved** and are protected by the unsaved-changes prompt from the moment they exist.
- Opened files, new files and recordings all work the same way after they are created.

### Non-goals
- Recording into an existing file, punch-in or overdub. A take is always a new file.
- Starting a take from the input level (a level trigger), a metronome, or a count-in.
- Monitoring the input through Limewave. Musicians hear themselves through their interface.
- Recording or creating files longer than **10 minutes**. Longer audio is outside what this program is for.
- Line, curve, smoothing or shape tools for drawing, and drawing on several channels in one stroke.
- Turning a drawn shape into a tone in some other way, such as repeating it or resampling it. You hear exactly what you draw.
- Recording more than two channels into one file.

### Anti-goals
- **A stray click must never change audio.** Only a drag draws. A click in drawing mode does what a click does today: it sets the play-from point.
- **A take must never be lost silently.** An unsaved new file or recording counts as unsaved even before any edit. Closing the window, opening a folder or anything else that would drop it goes through the unsaved-changes prompt.
- **Recording must not drop audio** because the window is busy, such as while dragging, while a dialog renders, or while a large file opens. Minimizing the window, or moving or resizing it, must not drop audio either. It must also not glitch the playback engine.
- **The microphone is never opened unless the user asked.** The input stream exists only while the recording panel is open.
- No third-party program or product names anywhere in the code, UI text, docs or commit messages (standing project rule).

## 3. Drawing mode

### 3.1 The toggle
- A **Draw** toggle on the toolbar, next to Snap and Lock length. Its label is a pencil glyph, if the app's font has one, followed by "Draw". Hover text: "Draw the waveform with the mouse (each stroke is one undo step)".
- The toggle state is per window, not per file, and is not saved. It starts off. **New…** switches it on (§4.3).
- **Escape** turns Draw off when nothing else uses Escape that frame. Escape already clears the selection and play-from point, and closes menus first.

### 3.2 What a drag does while Draw is on
- **Markers still win.** Pressing a marker line or flag drags the marker as today.
- **Anywhere else on a channel lane starts a stroke on that lane's channel.** The pencil draws at the pointer's height, so the first value is the pointer's height, not the sample under it. The pointer x maps to frames as everywhere else, and values stop at full scale.
- **Filling:**
  - Every frame between one pointer position and the next is set on a straight line between the two values.
  - When one pixel covers many frames, all of them are set. Nothing is skipped and nothing is averaged.
  - Going back over frames already drawn in the same stroke redraws them.
- **One undo step per stroke.** On release, the stroke is applied with `redraw_samples` over the full range of frames it touched. The stroke is drawn live while dragging (§3.4). Playback and the overview strip catch up on release.
- **No selection drag while Draw is on.** Shift-click still extends a selection, Ctrl/Cmd+A still selects Start…End, and the Edit menu still works.
- **Shift (fine control) applies only to grabbing a dot with Draw off.** With Draw on, the pencil is always under the pointer.
- A press outside every lane, such as on the ruler, starts nothing.

### 3.3 With Draw off
Nothing changes from today:
- dragging empty space selects;
- when zoomed in, dragging a sample dot moves it, sideways redraws, and Shift gives fine control.

### 3.4 Drawing feedback
- While Draw is on, the cursor over a lane is a crosshair. Over a marker it stays the resize arrow.
- **The stroke shows as it is drawn at every zoom:**
  - Zoomed in, the dots and line follow the stroke, as the pencil does now.
  - Zoomed out, the bars of the columns the stroke covers are redrawn from the stroke's values.
- **Responsiveness:**
  - The view stays responsive (a frame under ~16 ms on an ordinary machine) while a stroke covers up to a few seconds of audio at 44.1 kHz.
  - The undo step may take longer on release for a stroke over minutes of audio, but should stay under half a second for a full 10-minute mono file.

### 3.5 Memory
Each stroke stores a copy of every channel over the frames it touched; that is how `Splice` steps work. A stroke across a whole 10-minute stereo file stores about 200 MB. That is accepted: such strokes are rare, and undo frees them when the history is cut.

## 4. New blank sample

### 4.1 The dialog
- A **New…** button at the start of the toolbar, before "Open file…".
- It opens a modal dialog, styled like the effect dialogs, with these fields:

| Field | Values | Default |
| --- | --- | --- |
| Length | A number with a unit: frames or milliseconds | 1000 ms |
| One cycle of… | A note entry, such as `A3` or `57`. Filling it sets Length to one cycle of that note, rounded to whole frames, and shows the true pitch of that length in cents. | empty |
| Sample rate | 22050, 32000, 44100, 48000, 88200, 96000 Hz | 44100 |
| Channels | Mono, Stereo | Mono |
| Bit depth | 16-bit, 24-bit, 32-bit float | 16-bit |
| Root note | A note entry | C4 (MIDI 60) |
| Loop the whole file | Checkbox | Off; switched on whenever "One cycle of…" sets the length |

- **Root note:** filling "One cycle of…" also sets the root note to that note. Editing Length by hand afterwards clears "One cycle of…" but leaves the root note.
- **Limits and actions:**
  - Length is limited to 1 frame up to 10 minutes. Out-of-range input shows the limit beside the field, and Create stays disabled.
  - **Create** makes the file. **Cancel**, Escape or a click outside closes the dialog.

### 4.2 What Create makes
- A silent document of that length, rate, channel count and bit depth.
- **Markers and loop:**
  - Start is frame 0 and End is the last frame.
  - With "Loop the whole file", a forward loop runs over every frame (loop start 0, loop end last frame).
  - Otherwise there is no loop.
- **The root note** goes in the `smpl` chunk on save, like any root-note edit.
- **Name:** it is named **Untitled 1**, **Untitled 2** and so on, counting up for the whole run of the program. The name shows in the file list and window title.
- **The file list:** the file is added and selected. If the window was showing one opened file on its own, it switches to the list view so both are reachable.

### 4.3 After Create
- Draw switches on, and the view shows the whole file.
- The new file counts as unsaved (§6.2).

## 5. Recording

### 5.1 The panel
- A **Record** button on the toolbar, next to Draw.
  - It is disabled, with hover text "No audio input device found", when the system lists no input devices.
  - Otherwise it opens a modal **Record** panel.
- **The panel shows:**
  - **Input device:** a list of the system's input devices, with the system default chosen first. The list is read again each time the panel opens.
  - **Channels:** which input channels to record. The choices depend on the device:
    - one channel: "Mono";
    - two or more channels: each single input ("Input 1", "Input 2", and so on), then each adjacent pair ("Inputs 1+2", "Inputs 3+4", and so on).
    - Default: "Inputs 1+2" when there are two or more, else "Mono".
    - A single input gives a mono file and a pair gives a stereo file.
  - **Bit depth** for the saved file: 16-bit, 24-bit or 32-bit float, defaulting to 16-bit.
  - The **sample rate** is the device's own rate, shown read-only ("48000 Hz").
  - **A level meter** for each chosen channel:
    - peak, in dBFS, with a short hold;
    - live from the moment the panel opens, so levels can be set before recording.
    - A **clip light** for each channel lights at or above −0.1 dBFS and stays lit until the next take starts or the light is clicked.
  - **Elapsed time** and the time left before the 10-minute cap.
  - **Buttons:** **Record**, then **Stop** while recording, and **Close**.
- **While recording**, device and channel choices are locked, and Close asks "Stop and keep the take?" with the choices Keep, Discard and Cancel.
- **Escape and clicking outside** do nothing while recording. Otherwise they close the panel like Close.

### 5.2 A take
- **Record** starts capturing the chosen channels at once. **Stop** ends the take.
- **The 10-minute cap:** recording stops by itself at exactly 10 minutes of audio. The panel says "Recording stopped at the 10-minute limit".
- **What a finished take becomes:** if it has at least one frame, it becomes a new untitled document:
  - named **Recording 1**, **Recording 2** and so on, counting up for the whole run;
  - at the device's rate, the chosen bit depth and one or two channels;
  - with Start and End over the whole take, no loop, and the root note C4 (MIDI 60).
- **After a take:**
  - The panel stays open, ready for another take.
  - Each take is added to the file list and selected, and the list view is shown as in §4.2.
  - The view of a new take shows the whole file.
- A take with no frames, from pressing Stop at once, makes nothing.
- **Playing notes:** the panel is modal like every other dialog, so the computer keyboard, the on-screen keyboard and the Play buttons are off while it is open.
  - Notes from a MIDI keyboard still sound, as they do with any dialog open.
  - Limewave doesn't route its own output to the input, but a microphone may pick it up; that is the user's business.

### 5.3 Capture and errors
- **Streams:** the input stream is opened when the panel opens, or when the device changes, and closed when the panel closes. It is separate from the output stream, which keeps running.
- **Capture path:**
  - The audio callback only copies samples into a lock-free buffer and updates the meter peaks.
  - A **capture thread**, not the UI, drains that buffer into the growing take. A minimized or stalled window therefore never loses audio.
  - The UI only reads the take's length and the meter, and receives the finished take.
  - The buffer holds at least **2 seconds** of audio.
  - If it overflows anyway, the take is stopped and kept, with the message "Recording stopped: audio was lost while the program was busy". Audio is never dropped silently.
- **Device errors:**
  - If the device fails or disappears during a take, what was captured is kept as a take, and the panel shows the error.
  - If the device cannot be opened, the panel shows why and Record is disabled until another device is chosen.
- **Format:** the device's native sample format is converted to `f32` in the callback. The rate is never resampled.
- **macOS:** recording needs the microphone permission text in the app bundle's `Info.plist` (`NSMicrophoneUsageDescription`: "Limewave records audio from your input device when you press Record."). Where the bundle is built, it must include this.

## 6. Documents with no file yet

New files and recordings have no original file on disk, and the code assumes every document has one: saving patches the original's bytes, `doc.path` names the file, and Save all writes into a folder next to the original. This section sets how untitled documents behave.

### 6.1 The document
- An untitled document holds its audio and format, with no original file.
- It needs a stand-in path for anything that shows or compares paths, for example `Untitled 1.wav`, with no folder.
- Undo history, effects, the pencil, markers, the loop, the root note and every effect work on it as on an opened file.
- It has no extra chunks, cues or `smpl` loops to keep.

### 6.2 Unsaved state
- An untitled document counts as **unsaved from the moment it exists**, even with no edits. It counts in the unsaved-changes prompt and in the "unsaved" marking in the file list.
- After its first successful save it becomes an ordinary file at the saved path. Later edits are dirty or clean exactly as for an opened file.

### 6.3 Saving
- **Save** on an untitled document opens **Save as…**.
- Save as… for an untitled document:
  - asks for a **folder** and a **file name**, prefilled with the document's name plus `.wav`;
  - uses the format the document was made with.
- **What gets written** is a fresh WAV:
  - `fmt `, `data`, `fact` when the format needs it;
  - `smpl`, **always**, with the root note and the loop if there is one. An untitled document counts as having a root note set, so new files and recordings always carry it. This differs from opened files, which only get `smpl` when they had one or their root, tuning or loop changed;
  - the usual `ISFT` tag.
- **Existing files:** an existing file of that name is backed up first, like the Folder destination today.
- **The batch Save as… dialog:** untitled documents never take part in the folder-batch save ("All files in folder", "Only changed", "Overwrite originals"). They are saved one at a time:
  - Save as… with an untitled document selected uses the **system save-file picker**, which handles folder, name and overwrite confirmation.
  - The batch dialog leaves untitled documents out of its list, and says "Untitled files are saved one at a time with Save as…" when any are open.
- **Save all** and the unsaved prompt's "Save all":
  - An untitled document with no destination opens Save as… for each one in turn.
  - Choosing Cancel in any of those stops the held action, so nothing is discarded.
- **After the first save**, the document behaves as if it had been opened from the saved file. Saving again goes to the same place, as `ReplaceSaved` does today.

## 7. Edit model
- **No new kind of edit.** Drawing uses the existing `Splice` step of kind `Draw` (`redraw_samples`). It keeps the length, is not tinted, and is replayed, mapped and saved like any splice.
- **Starting state:** a new or recorded document's original audio is the silence or the take itself, and its history starts with the default edit (Start…End over the whole file, plus the loop from §4.2 if chosen). Undo cannot go back past creation.
- Nothing in v0.3's length handling changes.

## 8. Testing

### 8.1 Core (`limewave-core`)
- A blank document of each channel count, rate and bit depth saves and reloads with:
  - the right length, all zero samples, and the right format;
  - `smpl` holding the root note, and the whole-file loop when chosen;
  - `ISFT` set.
- "One cycle of" length: A4 at 44100 Hz gives 100 frames, and C4 at 48000 Hz gives 183 frames. The cents readout matches `1200·log2(rate / (length · f_note))` to within 0.1 cent.
- A recorded take built from given `f32` frames saves at 16-, 24- and 32-bit float and reloads within the format's precision.
- Redrawing a range on an untitled document, then saving, writes the drawn values.

### 8.2 App (`limewave`, with the test harness)
- **Draw on, zoomed out:** a horizontal drag across a lane sets every frame between the two ends to the line between the two pointer heights, and only on that lane's channel. It is one undo step.
- **Draw on, zoomed in:** a press away from any dot starts drawing at the pointer's height.
- **Draw on:** a click with no drag changes no audio and sets the play-from point. A press on a marker still drags the marker.
- **Draw off:** the existing pencil, selection and marker tests still pass unchanged.
- **New…:**
  - Create adds an untitled, selected document with the chosen length and loop, and turns Draw on.
  - Out-of-range lengths disable Create.
  - "One cycle of A4" at 44100 Hz sets 100 frames, root note A4 and the whole-file loop.
- **Unsaved state:** an untitled document with no edits counts as unsaved. Opening a folder brings up the prompt. "Save all" opens Save as… for it, and cancelling that cancels the held action.
- **Recording**, with a **fake input source**:
  - The panel and capture logic are tested against a test source that feeds known frames in callback-sized blocks; no audio device is needed in tests.
  - Cases:
    - Record then Stop makes a "Recording 1" document with exactly the fed frames, on the chosen channels.
    - A single input gives mono and a pair gives stereo.
    - The cap stops at exactly 10 minutes of frames.
    - Overflowing the buffer stops and keeps the take, with the message.
    - A device error mid-take keeps what was captured.
    - Close while recording asks Keep, Discard or Cancel.
    - A zero-frame take makes nothing.
- **The meter:** the peak and the clip light follow the fed frames, and the clip light stays lit until the next take or a click.

### 8.3 Manual checks
- Record a short note from a real interface at 44.1 and 48 kHz. Check that the take plays back at the right pitch and speed and that nothing clicks or drops.
- Draw a single cycle on "New…" with "One cycle of C4", play it on the keyboard, and check it holds a steady pitch.
- Draw over a long opened file zoomed all the way out, and check the window stays responsive.

## 9. Calls I made (please check)
- **Version and roadmap:** this is **0.4.0**, and the SFZ export planned next moves to **0.5.0**.
- **Root note:**
  - The default root note is written as **C4 (MIDI 60)**, which is how Limewave names middle C. In conversation I called it "C3", but the intended note is MIDI 60.
  - New files and recordings both use it.
- **Default length** of a new file is 1000 ms.
- **Strokes start at the pointer:** with Draw on, a stroke starts at the pointer's height, not at the sample under it. That's what a pencil does, and it differs from grabbing a dot with Draw off.
- **Shift with Draw on** does nothing special; fine control is only for grabbing a dot with Draw off.
- **Clicks with Draw on** set the play-from point as today, and selection dragging is off.
- **Escape** turns Draw off.
- **Channel choices:** recording offers each single input and each adjacent pair, so a multi-input interface can record inputs 3+4.
- **Bit depth for recordings** is chosen in the panel and defaults to 16-bit, the same as New….
- **Several takes:** the recording panel stays open after a take, so several takes can be made in a row.
- **Names:** new files are named "Untitled N" and recordings "Recording N".
- **Capture thread:** a background thread, not the UI, moves recorded audio into the take, so minimizing the window during a take is safe.
- **The Record panel is modal:** the computer keyboard, the on-screen keyboard and Play are off while it is open, and MIDI notes still sound.
- **`smpl` on untitled files:** new files and recordings always get `smpl` with their root note on save, so a sampler knows the pitch.
- **Saving untitled files:** they are saved one at a time with the system save-file picker, never in the folder-batch dialog.
- **Buffering:** the capture buffer holds 2 seconds, and an overflow stops and keeps the take rather than dropping audio.

## 10. Left to the builder
- **Recording internals:**
  - the lock-free buffer type;
  - how the meter's hold and decay look;
  - how to list devices and their channel counts through the audio library already in use.
- **Layout:**
  - the exact layout of the New… dialog and the Record panel;
  - the pencil glyph, falling back to the plain word "Draw" if the font lacks one.
- **Live drawing:** how the stroke's preview is drawn at low zoom, within the responsiveness target in §3.4.
- **Untitled documents:**
  - how `WavDocument` represents a document with no original bytes, such as an optional source;
  - how the save path writes a fresh file for one.
