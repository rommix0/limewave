//! Listing a folder's WAV files and loading them off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use eframe::egui;
use seamlime_core::document::{WavDocument, load_wav};

/// The `.wav` files directly inside `dir` (not recursive), sorted by file name. Unreadable
/// folders give an empty list.
pub fn wav_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
        .collect();
    files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    files
}

/// Loads `paths` in order on one worker thread. Each result is sent as `(index, result)` and
/// followed by a repaint request. The worker stops when the receiver is dropped.
pub fn load_in_background(
    paths: Vec<PathBuf>,
    ctx: egui::Context,
) -> Receiver<(usize, Result<WavDocument, String>)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (i, path) in paths.iter().enumerate() {
            let result = load_wav(path).map_err(|e| e.to_string());
            if tx.send((i, result)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_files_filters_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.WAV", "a.wav", "c.txt"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("d.wav"), b"x").unwrap();
        let names: Vec<_> = wav_files(dir.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.wav", "b.WAV"]);
    }

    #[test]
    fn wav_files_of_missing_folder_is_empty() {
        assert!(wav_files(Path::new("definitely/not/here")).is_empty());
    }
}
