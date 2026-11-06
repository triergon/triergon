//! Deterministic directory walks over the repository.

use std::fs;
use std::path::{Path, PathBuf};

/// Regular files under `root` whose path relative to `root` passes `keep`, in
/// sorted order. Build output (`target`), symbolic links and hidden directories
/// other than `.github` are not entered.
pub fn files(root: &Path, keep: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    visit(root, root, keep, &mut out);
    out.sort();
    out
}

fn visit(root: &Path, dir: &Path, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            if name == "target" || (name.starts_with('.') && name != ".github") {
                continue;
            }
            visit(root, &path, keep, out);
        } else if kind.is_file() && keep(path.strip_prefix(root).expect("walked under root")) {
            out.push(path);
        }
    }
}

/// A fresh, empty scratch directory for tests.
#[cfg(test)]
pub fn scratch(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "xtask-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}
