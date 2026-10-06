//! Collects the `.note` and `.png` files named on the command line.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Note,
    Png,
}

#[derive(Debug, Clone)]
pub struct Input {
    pub source: PathBuf,
    pub kind: InputKind,
    /// Path relative to the argument it came from; for a file argument just the file name.
    pub rel: PathBuf,
}

fn kind_of(path: &Path) -> Option<InputKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "note" => Some(InputKind::Note),
        "png" => Some(InputKind::Png),
        _ => None,
    }
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// `<stem>-page-NNN.png` (three or more digits): the name `notedown render` gives page
/// images. Returns `<stem>`.
fn render_stem(name: &str) -> Option<&str> {
    let base = name
        .strip_suffix(".png")
        .or_else(|| name.strip_suffix(".PNG"))?;
    let (stem, digits) = base.rsplit_once("-page-")?;
    (!stem.is_empty() && digits.len() >= 3 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(stem)
}

/// Folders and files notedown itself writes next to the inputs or under `--out`.
struct Generated<'a> {
    /// The canonical `--out` folder, when it exists.
    out: Option<&'a Path>,
}

impl Generated<'_> {
    fn is_out_dir(&self, dir: &Path) -> bool {
        self.out
            .is_some_and(|out| std::fs::canonicalize(dir).is_ok_and(|d| d == out))
    }
}

fn walk(
    dir: &Path,
    base: &Path,
    generated: &Generated,
    inputs: &mut Vec<Input>,
    warnings: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warnings.push(format!("skipped {}: {e}", dir.display()));
            return;
        }
    };
    let entries: Vec<_> = entries.flatten().collect();
    let names: HashSet<String> = entries
        .iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_hidden(&entry.file_name()) {
            continue;
        }
        // file_type does not follow symlinks: a link to a folder is never entered, so a
        // symlink loop cannot recurse; a link to a file is accepted like a file.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if name.ends_with(".assets") || generated.is_out_dir(&path) {
                continue;
            }
            walk(&path, base, generated, inputs, warnings);
            continue;
        }
        if file_type.is_symlink() && !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            warnings.push(format!(
                "skipped {}: symbolic link to a folder or to nothing",
                path.display()
            ));
            continue;
        }
        match kind_of(&path) {
            Some(InputKind::Png)
                if render_stem(&name)
                    .is_some_and(|stem| names.contains(&format!("{stem}.note"))) => {}
            Some(kind) => {
                let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                inputs.push(Input {
                    source: path,
                    kind,
                    rel,
                });
            }
            None => warnings.push(format!(
                "skipped {}: not a .note or .png file",
                path.display()
            )),
        }
    }
}

/// Expands files and folders into inputs sorted by path, plus warnings for skipped paths.
/// Inside folders, what notedown writes is skipped: `<name>.assets/` folders, the `out`
/// folder and page images `<stem>-page-NNN.png` next to `<stem>.note`.
pub fn collect(paths: &[PathBuf], out: Option<&Path>) -> (Vec<Input>, Vec<String>) {
    let mut inputs = Vec::new();
    let mut warnings = Vec::new();
    let out = out.and_then(|o| std::fs::canonicalize(o).ok());
    let generated = Generated {
        out: out.as_deref(),
    };
    for path in paths {
        if path.is_dir() {
            walk(path, path, &generated, &mut inputs, &mut warnings);
        } else if path.is_file() {
            match kind_of(path) {
                Some(kind) => inputs.push(Input {
                    source: path.clone(),
                    kind,
                    rel: path.file_name().map(PathBuf::from).unwrap_or_default(),
                }),
                None => warnings.push(format!(
                    "skipped {}: not a .note or .png file",
                    path.display()
                )),
            }
        } else {
            warnings.push(format!(
                "skipped {}: no such file or folder",
                path.display()
            ));
        }
    }
    inputs.sort_by(|a, b| a.source.cmp(&b.source));
    (inputs, warnings)
}

/// The Markdown path of every input: `<rel without extension>.md` under `out` or next to
/// the source. When two inputs map to the same path, the PNG one becomes `<name>.png.md`.
pub fn output_paths(inputs: &[Input], out: Option<&Path>) -> Vec<PathBuf> {
    let plain: Vec<PathBuf> = inputs
        .iter()
        .map(|i| match out {
            Some(out) => out.join(&i.rel).with_extension("md"),
            None => i.source.with_extension("md"),
        })
        .collect();
    let mut note_paths = HashSet::new();
    for (i, p) in inputs.iter().zip(&plain) {
        if i.kind == InputKind::Note {
            note_paths.insert(p.clone());
        }
    }
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(plain.len());
    for (i, p) in inputs.iter().zip(plain) {
        let clash = match i.kind {
            InputKind::Png => note_paths.contains(&p) || seen.contains(&p),
            InputKind::Note => false,
        };
        let p = if clash {
            let mut name = p.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
            name.push(".png.md");
            p.with_file_name(name)
        } else {
            p
        };
        seen.insert(p.clone());
        result.push(p);
    }
    result
}
