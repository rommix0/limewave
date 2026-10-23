# AGENTS.md

Notes for coding agents working on Limewave, an open-source loop and one-shot WAV sample editor in Rust. [README.md](README.md) covers what the app does and how to use it.

## Layout

- `crates/limewave-core`: the UI-free library.
  - WAV and `smpl` reading and writing, the edit model, and effects.
  - Effects are an undoable list of steps: `effects.rs` and `edit.rs`.
  - Tails are in `tail/`, SFZ instruments in `sfz/`, and the DSP in `dsp/`.
  - Everything here is pure functions on `&[f32]`, tested with unit tests.
- `crates/limewave`: the eframe/egui 0.37 app.
  - `app.rs` holds the app state, sessions and undo, and `ui/` the views and dialogs.
  - Audio and MIDI are in `audio/`.
  - Long renders run on a worker thread through `render_worker.rs`.
  - Tests in `tests/` drive the whole app with egui_kittest.
- `docs/superpowers/specs/` and `docs/superpowers/plans/` hold each version's design, implementation plan and manual checklist. The rest of `docs/` is git-ignored.

## Before you commit

Run these, the same as CI:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check licenses
```

A few app tests depend on timing and can fail when the machine is busy, for example `overwrite_makes_bak_files`. Rerun a failure on its own before you treat it as real.

## Rules

- **No third-party names.** Never name other audio programs, products or hardware in code, comments, UI text, docs or commit messages. Write "an old hardware sampler" or similar.
- **Licences:**
  - The project is MIT or Apache-2.0.
  - Add dependencies only under permissive licences; `cargo deny check licenses` must stay clean.
  - Prefer no new dependency at all.
- **Files not to commit:**
  - `samples/` holds third-party test audio and is git-ignored. Never commit it.
  - `session_memory.json` is a tool cache. Never stage it.
- **Text style:**
  - Every pub item gets a doc comment.
  - Match the surrounding code's comment density and naming.
  - Use British spelling in UI text and docs.
  - Line endings are LF (`.gitattributes`).
- **Commits:**
  - Use conventional messages: `feat(core): …`, `fix(app): …`, `docs: …`, `test(core): …`.
  - Commit and merge only when the maintainer asks. There is no remote.
- **Versioning:** the version lives in the workspace `Cargo.toml`. The `software_tag` test in `crates/limewave-core/src/lib.rs` pins it, so bump both, plus `Cargo.lock`.
- **Process:** new features start with a design in `docs/superpowers/specs/`, then a plan in `docs/superpowers/plans/`. A release ends with a manual checklist beside its plan.
