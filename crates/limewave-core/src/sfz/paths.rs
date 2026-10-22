//! Resolving the sample names an SFZ file mentions to files on disk.

use std::path::{Component, Path, PathBuf};

pub(crate) fn is_absolute_text(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// `path` with `.` dropped and `..` folded into the part before it, without touching the
/// disk. A `..` that has nothing to fold into is kept, except right after a root.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The text a sample resolves from: `/`-separated, `default_path` and the name joined unless
/// the name is absolute.
fn effective_text(default_path: &str, sample: &str) -> String {
    let sample = sample.replace('\\', "/");
    if is_absolute_text(&sample) {
        return sample;
    }
    let mut d = default_path.replace('\\', "/");
    if !d.is_empty() && !d.ends_with('/') {
        d.push('/');
    }
    d + &sample
}

/// Whether a sample resolves to a place under the folder holding the `.sfz`: its effective
/// text is relative and, once `.` and `..` are folded, does not climb out.
pub fn stays_inside(default_path: &str, sample: &str) -> bool {
    let text = effective_text(default_path, sample);
    !is_absolute_text(&text)
        && !normalize(Path::new(&text))
            .components()
            .any(|c| matches!(c, Component::ParentDir))
}

/// Path of a sample named in an instrument. `\` and `/` both separate folders;
/// an absolute sample name is kept, else `default_path` and the name are
/// joined onto the folder holding the `.sfz`. `.` and `..` are folded in the
/// text, so the result never depends on folders in between existing.
pub fn sample_path(sfz_dir: &Path, default_path: &str, sample: &str) -> PathBuf {
    let mut text = effective_text(default_path, sample);
    if is_absolute_text(&text) {
        // Collapse doubled separators but keep a leading one.
        while text.contains("//") && !text.starts_with("//") {
            text = text.replace("//", "/");
        }
        return normalize(Path::new(&text));
    }
    let mut path = sfz_dir.to_path_buf();
    for part in text.split('/').filter(|p| !p.is_empty() && *p != ".") {
        path.push(part);
    }
    normalize(&path)
}

/// The file at `path`: the exact one, else the one whose every component
/// matches ignoring case. `None` if missing, or a generator name like `*sine`.
pub fn find_sample(path: &Path) -> Option<PathBuf> {
    if path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('*'))
    {
        return None;
    }
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let mut found = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(name) => {
                let exact = found.join(name);
                if exact.exists() {
                    found = exact;
                    continue;
                }
                let dir = if found.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    found.as_path()
                };
                let want = name.to_string_lossy().to_lowercase();
                let hit = std::fs::read_dir(dir)
                    .ok()?
                    .filter_map(Result::ok)
                    .find(|e| e.file_name().to_string_lossy().to_lowercase() == want)?;
                found.push(hit.file_name());
            }
            other => found.push(other.as_os_str()),
        }
    }
    found.is_file().then_some(found)
}

/// A key for "the same file named two ways": lowercased, `/`-separated, with
/// doubled separators and `.` parts dropped.
pub fn same_file_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/").to_lowercase();
    let parts: Vec<&str> = text
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let lead = if text.starts_with('/') { "/" } else { "" };
    format!("{lead}{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backslash_paths_resolve() {
        let dir = Path::new("base");
        assert_eq!(
            sample_path(dir, "", "Samples\\a b.wav"),
            dir.join("Samples").join("a b.wav")
        );
        // `..` is folded in the text, so `base` need hold no folder to climb out of.
        assert_eq!(sample_path(dir, "", "../x.wav"), PathBuf::from("x.wav"));
        assert_eq!(sample_path(dir, "", "a/../b.wav"), dir.join("b.wav"));
        assert_eq!(
            sample_path(Path::new("D:/Bank/Inst"), "Kit/", "../../shared/y.wav"),
            PathBuf::from("D:/Bank/shared/y.wav")
        );
    }

    #[test]
    fn dots_fold_lexically() {
        assert_eq!(
            normalize(Path::new("a/./b/../c")),
            PathBuf::from("a").join("c")
        );
        assert_eq!(normalize(Path::new("../a")), PathBuf::from("..").join("a"));
        assert_eq!(normalize(Path::new("a/../..")), PathBuf::from(".."));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
    }

    #[test]
    fn stays_inside_only_for_relative_texts_that_do_not_climb() {
        assert!(stays_inside("", "Samples\\x.wav"));
        assert!(stays_inside("Kit/", "../Kit/x.wav"));
        assert!(stays_inside("", "a/../b.wav"));
        assert!(!stays_inside("", "../x.wav"));
        assert!(!stays_inside("Kit/", "../../x.wav"));
        assert!(!stays_inside("../Samples/", "x.wav"));
        assert!(!stays_inside("", "/abs/x.wav"));
        assert!(!stays_inside("", "C:\\abs\\x.wav"));
        // An absolute default path points anywhere, even into the folder.
        assert!(!stays_inside("C:/inst/Kit/", "x.wav"));
        assert!(!stays_inside("/inst/Kit/", "x.wav"));
    }

    #[test]
    fn default_path_prefixes() {
        let dir = Path::new("base");
        let want = dir.join("Samples").join("a.wav");
        assert_eq!(sample_path(dir, "Samples\\", "a.wav"), want);
        assert_eq!(sample_path(dir, "Samples/", "a.wav"), want);
        assert_eq!(sample_path(dir, "Samples", "a.wav"), want);
    }

    #[test]
    fn absolute_samples_are_kept() {
        let dir = Path::new("base");
        assert_eq!(
            sample_path(dir, "Samples/", "/abs/a.wav"),
            PathBuf::from("/abs/a.wav")
        );
        assert_eq!(
            sample_path(dir, "Samples/", "C:\\abs\\a.wav"),
            PathBuf::from("C:/abs/a.wav")
        );
    }

    #[test]
    fn case_insensitive_match_finds_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("Samples")).unwrap();
        std::fs::write(tmp.path().join("Samples").join("EP.wav"), b"x").unwrap();
        let found = find_sample(&tmp.path().join("samples").join("ep.wav")).unwrap();
        // A case-insensitive file system may hand the path back as given.
        assert_eq!(
            found.file_name().unwrap().to_string_lossy().to_lowercase(),
            "ep.wav"
        );
        assert!(found.is_file());
        assert!(find_sample(&tmp.path().join("samples").join("nope.wav")).is_none());
        // An exact path is returned as given.
        let exact = tmp.path().join("Samples").join("EP.wav");
        assert_eq!(find_sample(&exact), Some(exact));
    }

    #[test]
    fn generator_samples_are_missing() {
        assert!(find_sample(&sample_path(Path::new("."), "", "*sine")).is_none());
    }

    #[test]
    fn same_file_key_ignores_case_and_separators() {
        let dir = Path::new("D:/Bank");
        let a = sample_path(dir, "", "Samples\\EP A1 mp.wav");
        let b = sample_path(dir, "", "samples/ep a1 mp.wav");
        assert_eq!(same_file_key(&a), same_file_key(&b));
        assert_ne!(
            same_file_key(&a),
            same_file_key(&sample_path(dir, "", "samples/ep a2 mp.wav"))
        );
    }
}
