# Limewave SFZ instruments: design

Status: draft for approval · 2026-10-22

This document specifies version **0.5.0**. It makes an **SFZ instrument** something Limewave opens, edits, auditions and saves. It is roadmap item M1 from the v0.1 design (§9), which the v0.2 design moved behind tails and the v0.4 design moved behind drawing and recording.

It builds on the v0.1–v0.4 designs (`2026-10-19-limewave-design.md`, `2026-10-20-limewave-v0.2-design.md`, `2026-10-21-limewave-tails-design.md`, `2026-10-22-limewave-drawing-and-recording-design.md`), which stay the reference for everything not changed here. Where they disagree, this document wins.

The user skipped the question round and asked to go straight to planning, so every decision below that isn't in the earlier designs is **my call** and is listed in §11 for checking.

## 1. Intent

Musicians rarely use one sample on its own. A playable instrument is a set of samples spread across the keyboard, often in velocity layers, and SFZ is the open, plain-text format that describes one. Today Limewave can loop and tune every WAV in a set, but the instrument file that ties them together has to be written by hand or in another program.

This milestone lets a musician:
- **open an existing `.sfz`** and see its regions as a list and as a key-by-velocity map;
- **fix what's wrong** in it: loops, tuning, key and velocity ranges, levels and envelopes;
- **hear it** from the computer keyboard, the on-screen keyboard or a MIDI keyboard, with the right sample sounding for each key and velocity;
- **build a new instrument** from a folder of WAVs, with key ranges laid out from the root keys;
- **save it** as an `.sfz` with its WAVs, without damaging anything Limewave doesn't understand.

The test library for this milestone is `samples/RSB 501-01/` (local only, never committed): three instruments ripped from an old hardware sampler, sharing 16 mono 16-bit WAVs at 30 kHz and 15 kHz. It shows the cases that matter:
- one WAV used by two regions with different key ranges;
- the instrument's `pitch_keycenter` and `tune` disagreeing with the root key stored in the WAV;
- two groups layered over the same keys with no velocity split;
- many settings Limewave doesn't edit (filter and pitch envelopes, LFOs, `transpose`, `offset`);
- Windows-style `\` paths and sample names with spaces.

## 2. Goals, non-goals, anti-goals

### Goals
- Open `.sfz` files from **Open file…** and from a folder's file list.
- Show an open instrument as a **region list** grouped like its `<group>`s, and a **key map** along the bottom.
- Edit each region's **key range, velocity range, volume, playback mode and amplitude envelope**, and each sample's loop and tuning with the editor that already exists.
- **Audition the whole instrument**: a note plays every region whose key and velocity range contain it, at the right pitch and level.
- **Build a new instrument** from the WAVs open in folder mode.
- **Auto-map** regions from their root keys, and **fill gaps** between them.
- **Save** the `.sfz` plus its WAVs, to a new folder or over the originals with backups, optionally also writing `smpl` (loop and root key) into each WAV.

### Non-goals
- Following `#include`. An instrument that uses it is refused with a clear message (§3.1).
- Editing opcodes other than those listed in §3.4. Everything else is kept exactly and shown read-only.
- Auditioning filters, pitch and filter envelopes, LFOs, effects, round robins, crossfaded layers, release triggers, keyswitches or controllers. These are kept on save but are not heard in Limewave (§6.3).
- Editing `<global>`, `<master>`, `<control>` or `<group>` opcodes. Edits are written at region level (§7.2).
- Moving regions between instruments, more than one instrument open at once, and drag and drop.
- Other instrument formats (unchanged from v0.1).

### Anti-goals
- **Losing what Limewave doesn't understand.** Every header, opcode and comment Limewave doesn't edit is written back exactly, with its value text untouched, in its original order. Opening and saving an instrument without edits gives the same instrument.
- **Silently changing what an instrument sounds like.** An opcode is only written or changed because the user changed the thing it controls.
- **Damaging originals.** As for WAVs: nothing is replaced unless the user chooses to overwrite, and then with numbered `.bak` backups.
- **Breaking sample paths.** A saved instrument must find all its samples, wherever it was saved.
- No third-party program or product names anywhere in the code, UI text, docs or commit messages (standing project rule).

## 3. Reading SFZ

### 3.1 Syntax
The parser reads the SFZ text format as commonly written:
- **Headers**: `<control>`, `<global>`, `<master>`, `<group>`, `<region>`, and any other `<name>` (such as `<curve>`, `<effect>`, `<midi>`), which is kept but not interpreted.
- **Opcodes**: `name=value`. A value runs from after `=` up to the start of the next `name=` token, the next header, the next comment or the end of the line, with surrounding spaces trimmed. This is what lets `sample=Samples\EP A1 mp.wav` contain spaces.
- **Comments**: `//` to the end of the line, and `/* … */`. Both are kept.
- **`#define $NAME value`** is applied when reading values. The `#define` line itself is kept as written, and values that used a `$NAME` are written back with the `$NAME`, not the expansion.
- **`#include`** is not followed. Opening such a file shows "This instrument uses #include, which Limewave can't open yet" and opens nothing.
- **Encoding**: UTF-8; invalid bytes are read as Latin-1, so files from old tools still open. Line endings (`\n` or `\r\n`) are detected and kept on save.
- A file that has no `<region>` opens as an empty instrument.
- Malformed input never panics. An unknown or misspelled opcode is just kept.

### 3.2 Inheritance
A region's effective value for an opcode is its own, else its `<group>`'s, else its `<master>`'s, else the `<global>`'s, else the SFZ default. `<control>`'s `default_path` is prefixed to `sample=` paths.

### 3.3 Sample paths
- `sample=` is relative to the `.sfz` file's folder (after `default_path`). Absolute paths are allowed.
- `\` and `/` are both separators on every platform.
- If no file has the exact name, a case-insensitive match in the same folder is used, so instruments made on Windows open on Linux.
- A region whose sample can't be found is shown as **missing** in the list and on the map. It stays in the instrument and is saved as written, but it can't be selected for editing or heard.
- Built-in generator samples (names starting with `*`, such as `*sine`) count as missing in the same way.
- Each WAV is loaded once, however many regions use it.

### 3.4 Opcodes Limewave understands
These are read, auditioned and editable. Everything else is kept and listed read-only (§5.3).

| Opcode | Meaning | Default |
|---|---|---|
| `sample` | WAV file | — |
| `lokey`, `hikey`, `key` | key range; `key` sets both and `pitch_keycenter` | 0, 127 |
| `lovel`, `hivel` | velocity range | 1, 127 |
| `pitch_keycenter` | root key | 60 |
| `tune` | cents added to playback pitch | 0 |
| `transpose` | semitones added to playback pitch (auditioned, read-only) | 0 |
| `volume` | dB | 0 |
| `amp_veltrack` | % velocity affects level (auditioned, read-only) | 100 |
| `offset`, `end` | first and last frame played (auditioned, read-only) | 0, last |
| `loop_mode` | `no_loop`, `one_shot`, `loop_continuous`, `loop_sustain` | `loop_continuous` if the sample has a loop, else `no_loop` |
| `loop_type` | `forward`, `backward`, `alternate` | `forward` |
| `loop_start`, `loop_end` | loop frames, end inclusive | the WAV's `smpl` loop 0 |
| `ampeg_attack`, `ampeg_decay`, `ampeg_sustain`, `ampeg_release` | amp envelope, seconds and % | 0, 0, 100, 0 |

- Key opcodes accept MIDI numbers or note names (`c4`, `c#4`, `db4`), with middle C = `c4` = 60, the same naming Limewave already uses.
- `pitch_keycenter=sample` means "use the WAV's root key".
- `loopmode`, `loopstart`, `loopend` (older spellings) are read the same as `loop_mode`, `loop_start`, `loop_end` and written back under the spelling the file used.

## 4. The document model

### 4.1 Instrument, regions and samples
- An **instrument** is the parsed file: its headers, opcodes, comments and defines in order, plus the path it came from.
- A **region** is a `<region>` header. It refers to a **sample** by path.
- A **sample** is a WAV opened exactly as Limewave opens WAVs today, with its own editor state and undo history. Two regions that use the same WAV share one sample.

### 4.2 Who owns what
The rule is: **the sample owns the audio, the loop and the tuning; the region owns where and how it plays.**

- **Sample** (shared by every region that uses it): the audio and every audio edit (crossfade, trims, effects, tails, drawing), the loop points, the loop direction (forward, backward or ping-pong), the root key and fine tune.
- **Region** (its own): key range, velocity range, volume, playback mode (normal, one-shot, sustain loop), and amp envelope.

On opening, each sample's root key, fine tune and loop are taken from the instrument, not from the WAV, because the instrument is what a sampler plays:
- root key from the region's effective `pitch_keycenter` (unless it is `sample`);
- fine tune from `tune`: **fine tune = −tune**. Limewave's fine tune says how sharp the recording is, while `tune` says how much to raise playback, so a region with `tune=14` is a recording 14 cents flat (fine tune −14 ¢). This sign is tested;
- loop points from `loop_start`/`loop_end` if set, else from the WAV;
- loop direction from `loop_type`, and loop on or off from `loop_mode` (`no_loop` and `one_shot` turn the sample's loop off in the editor; `loop_continuous` and `loop_sustain` turn it on).

Taking these values is not an edit: the sample shows as unchanged after opening. The WAV's own root key, fine tune and loop (as v0.1 reads them) are remembered too, for writing WAVs without `smpl` changes (§7.3).

### 4.3 Regions that disagree
If two regions use the same WAV but their effective values for root key, fine tune or loop differ, the **first region** (in file order) sets the sample's values. Each later region that disagrees is marked as **keeping its own tuning and loop**:
- its `pitch_keycenter`, `tune`, `loop_*` opcodes are written back exactly as read, whatever is done to the sample;
- the inspector shows "This region keeps its own tuning and loop" for it, with those values read-only;
- it is auditioned with its own values.

Regions that agree **follow the sample**. This keeps the shared-sample case simple (one loop to edit, as the user sees one waveform) while never overwriting a deliberate difference.

### 4.4 New instruments
**New instrument** (toolbar, folder mode only, enabled when at least one WAV is open) makes an untitled instrument with one `<group>` and one region per open WAV, in file-list order, each with `pitch_keycenter` from the WAV's root key, then runs **Auto-map** (§5.5) on it. Like other untitled documents it is unsaved from the start and is saved through the save-file picker (§7). Its WAVs are not changed by this.

### 4.5 One instrument at a time
- One instrument is open at a time. Opening another, or opening a folder or WAV, while it has unsaved changes goes through the existing unsaved-changes prompt.
- While an instrument is open, the file list is replaced by the region list (§5.1), and the instrument's samples are the open files.
- Closing the instrument (**Close instrument** in the region list header) returns to the folder or file that was open before, if any.

## 5. The window with an instrument open

### 5.1 Region list (left panel)
- A header: the instrument's name, "N regions · M groups", an unsaved dot, and **Close instrument**.
- One section per `<group>`, titled by its `group_label` if it has one, else "Group 1", "Group 2"… in order, followed by its velocity range if the group sets one. Regions before the first `<group>` go in an untitled section at the top.
- A row per region: sample file name, root key name, key range ("F#2–A#2", or one note), a loop glyph if it loops, an unsaved dot if the region or its sample has unsaved changes, and "missing" if its sample is missing.
- Clicking a row selects the region: its sample opens in the editor, and the key map highlights it.
- **Unassigned WAVs** (a collapsed section at the bottom): WAVs in the folders the instrument's samples come from that no region uses. A row's **Add** button adds a region for it to the selected region's group (or the last group), with `pitch_keycenter`, `lokey` and `hikey` at the WAV's root key.
- **Add group** adds an empty `<group>` at the end. **Delete region** (a button shown for the selected region) removes it; its WAV moves to Unassigned if no other region uses it.

### 5.2 Key map (bottom panel)
The key map replaces the keyboard strip while an instrument is open. It covers all 128 keys (C-1 to G9), with a keyboard along its bottom edge that plays like the existing strip. The panel is about 160 px tall and can be resized.
- **Keys × velocity** (default): above each key range, a box per region spanning its velocity range, velocity up.
- **Keys only**: a box per region spanning its key range, stacked in rows when regions overlap.
- Boxes are tinted by group. The selected region is outlined. Overlapping boxes are drawn see-through so both show.
- Keys no region covers (at any velocity) are hatched.
- When a note plays, the boxes of the regions sounding light up.
- **Click** a box to select its region. Clicking again where several overlap selects the next one under the pointer.
- **Drag a box's left or right edge** to change its key range; **top or bottom edge** (in Keys × velocity) to change its velocity range. Edges snap to whole keys and velocities and can't cross (lo ≤ hi).
- **Right-click-drag across the keyboard** sets the selected region's key range to the keys dragged over. This is the meaning v0.2 reserved for it. Right-click without a drag still sets the root key, as today.
- A **Velocity** control (1–127, default 100) sets the velocity of notes played from the computer keyboard and the on-screen keys. MIDI notes use their own velocity.
- Buttons: **Auto-map from root keys**, **Fill gaps** (§5.5).

### 5.3 Inspector
With an instrument open, the inspector gains a **Region** section above the existing sections, for the selected region:
- **Keys**: low and high key, editable as note names.
- **Velocity**: low and high, 1–127.
- **Group**: the region's group, chosen from a list. Changing it moves the region to the end of that group.
- **Volume**: dB, −144 to +6.
- **Playback**: **Normal**, **One-shot** (plays the whole sample on a note, ignoring note-off; `loop_mode=one_shot`) or **Sustain loop** (loops while the key is held and plays on past the loop end on release; `loop_mode=loop_sustain`). Normal writes `loop_continuous` if the sample loops and `no_loop` otherwise.
- **Envelope**: the four ADSR values, edited with the same controls as the v0.2 envelope, but per region and saved as `ampeg_*`. SFZ gives times in seconds and sustain in percent; Limewave shows milliseconds and percent, as today. A region with no `ampeg_*` values uses the SFZ defaults (attack 0, decay 0, sustain 100 %, release 0), with the existing short release floor in audition only.
- **Other settings (kept as written)**: a collapsed read-only list of every other opcode that applies to the region (its own and inherited), with where each comes from (region, group, master, global).

The existing **Tuning** and **Loop** sections edit the region's sample, and say "Shared by N regions" when N > 1. For a region that keeps its own tuning and loop (§4.3), they show its values read-only.

In an instrument, the session-wide audition envelope and "Release plays after loop end" settings are hidden: each region's own envelope and playback mode take their place. They return when the instrument is closed.

### 5.4 Toolbar
- **Open…** accepts `.wav` and `.sfz`.
- A folder's file list shows `.sfz` files at the top, with an instrument glyph. Clicking one opens it (§4.5).
- **New instrument** (§4.4).
- **Save** and **Save as…** save the instrument (§7) while one is open.
- **Play** and the loop-play button play the selected region's sample, at its root key, as today.

### 5.5 Auto-map and Fill gaps
Both work on the selected region's group (every region in the instrument when it has no groups), as one undo step.
- **Auto-map from root keys**: sorts the group's regions by root key. Each region's range runs from just above the midpoint to its lower neighbour's root key up to the midpoint to its upper neighbour's root key, rounding the midpoint into the lower region. The lowest region reaches down to key 0 and the highest up to 127. Regions sharing a root key share the range. Velocity ranges are not changed.
- **Fill gaps**: within the group, each run of keys that no region covers (velocity is not considered) is split at its middle between the regions on either side, the lower region taking the middle key of an odd run; at the ends of the keyboard, the nearest region is extended. Existing ranges never shrink.

## 6. Audition

### 6.1 Which regions sound
A note with key `k` and velocity `v` plays every region whose effective `lokey ≤ k ≤ hikey` and `lovel ≤ v ≤ hivel`, whose sample is not missing, and whose `trigger` is `attack` (the default). Several regions can sound at once (layers). Each sounding region takes one voice from the existing pool.

When no instrument is open, audition works exactly as today.

### 6.2 How a region sounds
- **Pitch**: playback ratio = `2^((k − root + transpose)/12 + (tune)/1200) × sample rate / output rate`, where root and tune are the region's effective values (from its sample, or its own if it keeps them, §4.3).
- **Level**: `volume` dB, times the velocity curve: with `amp_veltrack` at 100 %, the gain is `(v/127)²`; at 0 %, velocity doesn't change the level; in between, the curve is mixed linearly in dB.
- **Start and end**: from `offset` to `end`.
- **Loop**: as the sample's loop and direction (or the region's own, §4.3). `no_loop` plays to the end; `one_shot` plays to the end and ignores note-off; `loop_continuous` loops until the envelope's release ends; `loop_sustain` loops while held and plays on past the loop end after note-off (the v0.3 "Release plays after loop end" behaviour, per region).
- **Envelope**: the region's `ampeg_*` values, through the existing ADSR. Release has the existing short floor so notes don't click off.
- Edits are heard live, as edits to a WAV are today.

### 6.3 Not auditioned
Everything else (filters, `fil*`, `pitcheg_*`, `fileg_*`, LFOs, `pan`, `width`, `seq_*`, `lorand`/`hirand`, `xf*`, keyswitches, CC-based opcodes, effects) is kept on save but not heard. The inspector's "Other settings" list marks these "not heard in Limewave".

## 7. Saving an instrument

### 7.1 The save dialog
The save dialog's format choice, shown disabled since v0.1, switches on while an instrument is open:
- **SFZ instrument** (the default with an instrument open), with a checkbox **Also write loop and root key into each WAV (`smpl`)**, off by default.
- **WAV files** saves the instrument's samples only, exactly as folder mode does today, and leaves the instrument unsaved.

With no instrument open, the dialog works as today.

**Destination** for SFZ:
- **New folder** (default): `<instrument folder>/limewave/`, as for WAVs. The `.sfz` is written there with its file name, and every sample the instrument uses is written or copied there at the same relative path, so the folder is complete on its own. A sample whose path climbs out of the instrument's folder (`..`) or is absolute is written into a `samples/` folder beside the saved `.sfz`, and its `sample=` is rewritten to match.
- **Overwrite originals**: the `.sfz` and every changed WAV are replaced in place, each with a numbered `.bak` backup first, as for WAVs today. Unchanged WAVs are left alone.
- Untitled instruments are saved through the save-file picker; `.sfz` is added if the name doesn't end in it. Samples go to the chosen folder by the New folder rule, with paths relative to the `.sfz`.

### 7.2 What the `.sfz` contains
The writer starts from the instrument as read and changes as little as possible:
- Headers, comments, defines and opcodes are written in their original order with their original value text. Layout is normalised: each header starts a line, opcodes follow one per line, and a blank line separates regions. *(Byte-identical layout is not kept; the content is.)*
- An edited opcode keeps its place and spelling; only its value changes. A new one is added at the end of its region.
- Edits are always written **at region level**. If a region inherits a value from its group and the user changes it, the region gets its own opcode; the group is unchanged.
- An opcode is written only when its value differs from what the region would get without it. Removing a region-level opcode that equals the inherited value is not done; existing opcodes are only ever changed, never removed, except with their region.
- For a sample whose root key, fine tune, loop or loop direction changed, every region that follows it (§4.3) gets explicit `pitch_keycenter`, `tune`, `loop_mode`, `loop_start` and `loop_end` (and `loop_type` unless forward), so the instrument plays right whether or not the WAVs carry `smpl`. Untouched samples' regions are not changed.
- If a sample is trimmed (Start marker moved), every region using it has `offset`, `end`, `loop_start` and `loop_end` shifted by the trim, clamped to the new length, so they point at the same audio.
- `sample=` keeps its original text unless the sample was moved by §7.1; new paths use `/`.
- Numbers are written plainly: integers without a decimal point, other values with up to 6 significant digits and no trailing zeros.
- Line endings follow the original file (new instruments use the platform's).

### 7.3 What the WAVs contain
A sample's WAV is written by the existing WAV save, with one change:
- **Also write loop and root key into each WAV** on: `smpl` is written from the sample's editor values, which came from the instrument (§4.2).
- Off: `smpl` is written from the **WAV's own** remembered values (§4.2), shifted by any trim exactly as v0.1 shifts loops. A WAV that had no `smpl` still gets none. So the WAV keeps saying what it said before, and the instrument carries the edits.

The audio, the crossfade and the trims are always written. A sample whose bytes would come out the same (no audio change, and nothing new for `smpl`) is copied byte for byte (New folder) or left alone (Overwrite), not re-encoded.

### 7.4 After saving
As for WAVs, the document continues from what was just written: the saved instrument is **reopened from disk**, with the same region selected, and undo history starts fresh. **Save** then repeats the last choices without the dialog, writing over that saved copy without backups (the v0.1 rule for a file Limewave saved itself).

## 8. Undo
- Region edits (ranges, group, volume, playback, envelope, add, delete, auto-map, fill gaps) go on the **instrument's** undo history. A drag on the key map is one step.
- Sample edits stay on the sample's history.
- Ctrl+Z undoes the **most recent** edit, whichever history it is on, and redo follows the same order. *(Each edit is stamped with a counter; undo picks the newest stamp among the instrument and the selected region's sample.)*
- The instrument counts as unsaved if its history has moved since the last save, or any of its samples is unsaved.

## 9. Edit model and core
- `limewave-core` gains an `sfz` module: parse, the opcode model with inheritance, sample-path resolution, the writer, and Auto-map / Fill gaps. No UI or audio code.
- The audio engine gains per-voice region parameters (buffer, ratio, gain, start, end, loop, playback mode, envelope), so a note can sound several regions with different samples. The v0.1 rules hold: no allocation, locking or blocking on the audio thread.
- Version **0.5.0**.

## 10. Testing

### 10.1 Core (`limewave-core`, synthetic files only)
- Parsing: headers, opcodes, values with spaces, `//` and `/* */` comments, `#define`, `#include` refused, note names, `key=`, `\r\n`, Latin-1 bytes, malformed input without panics.
- Inheritance through global, master and group.
- Round trip: parse → write → parse gives the same headers and opcodes, and comments are kept.
- Minimal writes: one edit changes one opcode; inherited values become region opcodes; untouched regions write back unchanged.
- Fine tune sign: `tune=14` reads as fine tune −14 ¢, and writes back as `tune=14`.
- Regions that share a sample and disagree (§4.3).
- Paths: `\` separators, `default_path`, case-insensitive match, missing samples, `..` paths moved into `samples/` on save.
- Trims shift `offset`, `end` and loop opcodes.
- Auto-map and Fill gaps on gaps, ties and single regions.

### 10.2 App (`limewave`, with the test harness)
- Opening an instrument shows the region list and key map; selecting a row or box selects the region and its sample.
- A note plays every matching region (layers and velocity splits), checked through the engine's voice commands.
- Dragging a box edge changes the range as one undo step; undo order across the instrument and a sample.
- Save to a new folder writes the `.sfz` and all samples, and the saved instrument reopens with the same regions; overwrite makes `.bak` files; the `smpl` checkbox on and off.
- New instrument from a folder of WAVs.
- The unsaved prompt covers instrument edits.
- An optional test, skipped when the folder is absent, opens the three `samples/RSB 501-01/` instruments and checks their region counts and that every sample is found.

### 10.3 Manual checks
A checklist file, as for earlier milestones: open each RSB instrument and play it across the keyboard; fix a loop in a shared sample and hear both regions change; save to a new folder and load it in a sampler; build an instrument from `samples/` WAVs.

## 11. Calls I made (please check)
- **The sample owns loop and tuning; the region owns mapping, level, playback mode and envelope** (§4.2). The editor keeps one waveform with one loop, even when several regions use it.
- **Regions that disagree keep their own values** read-only rather than being merged (§4.3).
- **The instrument's values win over the WAV's** on opening (§4.2), as in a sampler.
- **Fine tune = −tune** (§4.2).
- **Layout is normalised on save**; content, order, spelling and comments are kept (§7.2).
- **Edits are written at region level only**; group, master and global headers are never changed (§7.2).
- **`#include` is refused** rather than half-supported (§3.1).
- **`smpl` in WAVs is off by default** for SFZ saves, and when off the WAV's own `smpl` is kept as it was (§7.3).
- **One instrument at a time**, replacing the file list while open (§4.5).
- **Auto-map and Fill gaps work per group**, since groups are usually velocity layers (§5.5).
- **Velocity curve** `(v/127)²` at `amp_veltrack=100` (§6.2).
- **Not auditioned**: filters, modulation, round robins, crossfades, release triggers (§6.3).
- **New instrument** needs a folder of open WAVs; it doesn't change the WAVs (§4.4).
- **Samples outside the instrument folder** are gathered into `samples/` on a new-folder save (§7.1).

## 12. Left to the builder
- The key map's exact drawing, colours per group, edge grab width and the stacking order of overlapping boxes.
- How the parser stores spans or tokens, as long as §7.2 holds.
- How the engine gets region parameters to voices without locking.
- Which "Other settings" opcodes are labelled "not heard in Limewave" beyond those in §6.3.
