//! `@file` at the prompt (issue 19): the list of files and folders to pick from, and attaching
//! the text of the files a message names. A folder (`@src/`, issue 25) is attached as the list of
//! the files below it, not their text.
//!
//! Everything here works on paths relative to the working directory and never leaves it: no
//! absolute paths, no `..`, and a link that leads out is not followed.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// How many paths the list is made of at most.
pub const MAX_FILES: usize = 5000;
/// How deep below the working directory the list looks.
const MAX_DEPTH: usize = 8;
/// How many rows the picker shows.
pub const PICKER_ROWS: usize = 6;
/// The most text taken from one file.
pub const MAX_FILE_BYTES: usize = 20_000;
/// The most files named in the listing a folder is attached as.
pub const MAX_LISTED: usize = 200;
/// The most text attached to one message.
pub const MAX_TOTAL_BYTES: usize = 60_000;

/// Folders the list does not enter (dot-folders are skipped too).
const SKIPPED: &[&str] = &["node_modules", "target"];

/// The regular files and the folders below `root`, as paths relative to it, sorted; a folder ends
/// with `/`. Links are not followed.
pub fn files_under(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    walk(root, root, 0, &mut found);
    found.sort();
    found
}

fn walk(root: &Path, dir: &Path, depth: usize, found: &mut Vec<String>) {
    if depth > MAX_DEPTH || found.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if found.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !name.starts_with('.') && !SKIPPED.contains(&name.as_str()) {
                if let Ok(relative) = entry.path().strip_prefix(root) {
                    found.push(format!("{}/", relative.to_string_lossy()));
                }
                walk(root, &entry.path(), depth + 1, found);
            }
        } else if kind.is_file()
            && let Ok(relative) = entry.path().strip_prefix(root)
        {
            found.push(relative.to_string_lossy().into_owned());
        }
    }
}

/// The `@word` the cursor is in: where it starts, and the text after the `@` up to the cursor.
/// The `@` must start the line or follow whitespace.
pub fn mention(chars: &[char], cursor: usize) -> Option<(usize, String)> {
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    if chars.get(start) != Some(&'@') || start == cursor {
        return None;
    }
    Some((start, chars[start + 1..cursor].iter().collect()))
}

/// The files that fit what was typed after the `@`, best first: a case-insensitive substring of
/// the path, those that start with it first, then those whose file name does, then shorter paths.
pub fn matches(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let query = query.to_lowercase();
    let mut hits: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|path| {
            let lower = path.to_lowercase();
            // A folder already typed in full is not offered again; its files are.
            if !lower.contains(&query) || (lower == query && lower.ends_with('/')) {
                return None;
            }
            let name = lower.rsplit('/').next().unwrap_or(&lower);
            let rank = if lower.starts_with(&query) {
                0
            } else if name.starts_with(&query) {
                1
            } else {
                2
            };
            Some((rank, path.chars().count(), path))
        })
        .collect();
    hits.sort();
    hits.into_iter()
        .take(limit)
        .map(|(_, _, p)| p.clone())
        .collect()
}

/// One file that was attached to a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub path: String,
    /// The size of the file on disk.
    pub bytes: u64,
    /// Whether only the start of it was taken.
    pub cut: bool,
    /// For a folder: how many files its listing names. `None` for a file.
    pub listed: Option<usize>,
}

/// What [`attach`] made of a message.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Attachment {
    /// The message to send: the text as typed, then the files.
    pub message: String,
    pub files: Vec<Attached>,
    /// Why a named file was not attached.
    pub warnings: Vec<String>,
}

/// Attaches the text of every `@path` in `text` that names a regular file below `root`, and the
/// list of files of every one that names a folder. A word that names neither stays plain text.
/// Each distinct path is attached once.
pub fn attach(root: &Path, text: &str) -> Attachment {
    let mut out = Attachment {
        message: text.to_owned(),
        ..Attachment::default()
    };
    let Ok(canonical_root) = root.canonicalize() else {
        return out;
    };
    let mut done = BTreeSet::new();
    let mut total = 0usize;
    let mut blocks = String::new();
    for word in text.split_whitespace() {
        let Some(named) = word.strip_prefix('@') else {
            continue;
        };
        let Some(path) = candidates(named)
            .into_iter()
            .find(|c| is_plain_relative(c) && root.join(c).exists())
        else {
            continue;
        };
        if root.join(&path).is_dir() {
            // `src`, `src/` and `src/.` all name the same folder; `.` alone is not allowed.
            let parts: Vec<&str> = path
                .split('/')
                .filter(|piece| !piece.is_empty() && *piece != ".")
                .collect();
            if parts.is_empty() {
                continue;
            }
            let folder = format!("{}/", parts.join("/"));
            if !done.insert(folder.clone()) {
                continue;
            }
            match list_inside(&canonical_root, root, &folder) {
                Err(why) => out
                    .warnings
                    .push(format!("@{folder}: not attached ({why})")),
                Ok((listing, count, cut)) => {
                    if total + listing.len() > MAX_TOTAL_BYTES {
                        out.warnings.push(format!(
                            "@{folder}: not attached (over {MAX_TOTAL_BYTES} bytes for one message)"
                        ));
                        continue;
                    }
                    total += listing.len();
                    blocks.push_str(&format!(
                        "--- files in {folder} ---\n{listing}--- end of files in {folder} ---\n"
                    ));
                    out.files.push(Attached {
                        path: folder,
                        bytes: listing.len() as u64,
                        cut,
                        listed: Some(count),
                    });
                }
            }
            continue;
        }
        if !root.join(&path).is_file() || !done.insert(path.clone()) {
            continue;
        }
        match read_inside(&canonical_root, &root.join(&path), &path) {
            Err(why) => out.warnings.push(format!("@{path}: not attached ({why})")),
            Ok((content, bytes, cut)) => {
                if total + content.len() > MAX_TOTAL_BYTES {
                    out.warnings.push(format!(
                        "@{path}: not attached (over {MAX_TOTAL_BYTES} bytes for one message)"
                    ));
                    continue;
                }
                total += content.len();
                blocks.push_str(&format!("--- {path} ---\n{content}"));
                if !content.ends_with('\n') {
                    blocks.push('\n');
                }
                if cut {
                    blocks.push_str(&format!(
                        "[cut: {} more bytes]\n",
                        bytes as usize - content.len()
                    ));
                }
                blocks.push_str(&format!("--- end of {path} ---\n"));
                out.files.push(Attached {
                    path,
                    bytes,
                    cut,
                    listed: None,
                });
            }
        }
    }
    if !out.files.is_empty() {
        out.message = format!("{text}\n\nAttached files:\n\n{blocks}");
    }
    out
}

/// The word as typed, then without one trailing punctuation mark (`@a.txt,` names `a.txt`).
fn candidates(named: &str) -> Vec<String> {
    let mut all = vec![named.to_owned()];
    if let Some(trimmed) = named.strip_suffix([',', '.', ';', ':', '!', '?', ')', ']', '"', '\'']) {
        all.push(trimmed.to_owned());
    }
    all.retain(|c| !c.is_empty());
    all
}

fn is_plain_relative(path: &str) -> bool {
    !path.starts_with('/')
        && !path.starts_with('~')
        && !path.split('/').any(|piece| piece == "..")
        && !path.contains('\0')
}

/// The files below a folder of the working directory, one per line (at most [`MAX_LISTED`]), as
/// paths relative to the working directory. Returns the lines, how many files are named and
/// whether the list was cut.
fn list_inside(
    canonical_root: &Path,
    root: &Path,
    folder: &str,
) -> Result<(String, usize, bool), String> {
    let dir = root.join(folder);
    let real = dir
        .canonicalize()
        .map_err(|e| format!("cannot open {folder}: {e}"))?;
    if !real.starts_with(canonical_root) {
        return Err("it is a link that leads out of the working directory".to_owned());
    }
    let mut found = Vec::new();
    walk(root, &dir, 0, &mut found);
    found.retain(|p| !p.ends_with('/'));
    found.sort();
    let cut = found.len() > MAX_LISTED;
    let count = found.len().min(MAX_LISTED);
    let mut listing: String = found[..count].iter().map(|p| format!("{p}\n")).collect();
    if cut {
        listing.push_str(&format!(
            "[cut: more files not listed after {MAX_LISTED}]\n"
        ));
    }
    if found.is_empty() {
        listing.push_str("[no files]\n");
    }
    Ok((listing, count, cut))
}

/// Reads up to [`MAX_FILE_BYTES`] of a text file that really lies below the root. Returns the
/// text, the size on disk and whether it was cut.
fn read_inside(
    canonical_root: &Path,
    file: &Path,
    shown: &str,
) -> Result<(String, u64, bool), String> {
    let real: PathBuf = file
        .canonicalize()
        .map_err(|e| format!("cannot open {shown}: {e}"))?;
    if !real.starts_with(canonical_root) {
        return Err("it is a link that leads out of the working directory".to_owned());
    }
    let size = fs::metadata(&real).map_err(|e| e.to_string())?.len();
    let mut buffer = Vec::new();
    fs::File::open(&real)
        .and_then(|f| f.take(MAX_FILE_BYTES as u64).read_to_end(&mut buffer))
        .map_err(|e| format!("cannot read it: {e}"))?;
    let cut = size > buffer.len() as u64;
    let text = match std::str::from_utf8(&buffer) {
        Ok(text) => text,
        // The cut may have split the last character; anything else is not text.
        Err(e) if cut && e.error_len().is_none() => {
            std::str::from_utf8(&buffer[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return Err("it is not UTF-8 text".to_owned()),
    };
    if text.contains('\0') {
        return Err("it is not text".to_owned());
    }
    Ok((text.to_owned(), size, cut))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn put(dir: &Scratch, path: &str, text: &str) {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn the_list_is_sorted_relative_and_skips_dot_folders_and_build_folders() {
        let dir = Scratch::new("attach-list");
        put(&dir, "b.txt", "");
        put(&dir, "a/c.rs", "");
        put(&dir, ".git/config", "");
        put(&dir, "node_modules/x/y.js", "");
        put(&dir, "target/debug/z", "");
        put(&dir, "a/.hidden/q", "");
        assert_eq!(files_under(dir.path()), vec!["a/", "a/c.rs", "b.txt"]);
    }

    #[test]
    fn the_list_stops_at_the_depth_and_does_not_follow_links() {
        let dir = Scratch::new("attach-depth");
        put(&dir, "1/2/3/4/5/6/7/8/9/deep.txt", "");
        put(&dir, "1/2/3/4/5/6/7/8/ok.txt", "");
        put(&dir, "top.txt", "");
        std::os::unix::fs::symlink("/etc", dir.path().join("etc-link")).unwrap();
        std::os::unix::fs::symlink("top.txt", dir.path().join("file-link")).unwrap();
        let files = files_under(dir.path());
        assert!(
            files.contains(&"1/2/3/4/5/6/7/8/ok.txt".to_owned()),
            "{files:?}"
        );
        assert!(!files.iter().any(|f| f.contains("deep")), "{files:?}");
        assert!(!files.iter().any(|f| f.contains("link")), "{files:?}");
    }

    #[test]
    fn the_list_is_capped() {
        let dir = Scratch::new("attach-cap");
        for i in 0..(MAX_FILES + 50) {
            put(&dir, &format!("f{i:05}.txt"), "");
        }
        assert_eq!(files_under(dir.path()).len(), MAX_FILES);
    }

    #[test]
    fn a_mention_is_the_at_word_before_the_cursor() {
        let text = chars("look at @src/ma now");
        assert_eq!(mention(&text, 15), Some((8, "src/ma".to_owned())));
        assert_eq!(mention(&text, 9), Some((8, String::new())));
        // The cursor right before the @ is not in the word; nor is a plain word, or an @ inside one.
        assert_eq!(mention(&text, 8), None);
        assert_eq!(mention(&chars("a@b.com"), 7), None);
        assert_eq!(mention(&chars("hello"), 5), None);
        assert_eq!(mention(&chars("@x"), 2), Some((0, "x".to_owned())));
        assert_eq!(mention(&chars("@"), 1), Some((0, String::new())));
        assert_eq!(mention(&chars(""), 0), None);
    }

    #[test]
    fn matching_ranks_prefix_then_file_name_then_shorter_paths() {
        let files: Vec<String> = [
            "docs/main.md",
            "src/main.rs",
            "main.rs",
            "src/domain.rs",
            "README.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            matches(&files, "main", 6),
            vec!["main.rs", "src/main.rs", "docs/main.md", "src/domain.rs"]
        );
        assert_eq!(matches(&files, "MAIN", 2), vec!["main.rs", "src/main.rs"]);
        assert_eq!(matches(&files, "", 3).len(), 3);
        assert!(matches(&files, "zzz", 6).is_empty());
        assert_eq!(
            matches(&files, "src/", 6),
            vec!["src/main.rs", "src/domain.rs"]
        );
    }

    #[test]
    fn a_named_folder_is_attached_as_the_list_of_its_files() {
        let dir = Scratch::new("attach-folder");
        put(&dir, "src/main.rs", "fn main() {}");
        put(&dir, "src/deep/lib.rs", "x");
        put(&dir, "src/.git/config", "x");
        put(&dir, "other.txt", "x");
        for typed in ["@src", "@src/", "@src/."] {
            let a = attach(dir.path(), &format!("look at {typed}"));
            assert_eq!(
                a.message,
                format!(
                    "look at {typed}\n\nAttached files:\n\n--- files in src/ ---\nsrc/deep/lib.rs\nsrc/main.rs\n--- end of files in src/ ---\n"
                )
            );
            assert_eq!(a.files.len(), 1);
            assert_eq!(a.files[0].path, "src/");
            assert_eq!(a.files[0].listed, Some(2));
            assert!(a.warnings.is_empty());
        }
        // The same folder twice is listed once; an empty one says so.
        let a = attach(dir.path(), "@src @src/");
        assert_eq!(a.files.len(), 1);
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let a = attach(dir.path(), "@empty");
        assert!(a.message.contains("[no files]"));
    }

    #[test]
    fn a_long_folder_listing_is_cut_and_a_link_leading_out_is_refused() {
        let dir = Scratch::new("attach-folder-cut");
        for i in 0..MAX_LISTED + 5 {
            put(&dir, &format!("many/f{i:04}.txt"), "");
        }
        let a = attach(dir.path(), "@many");
        assert_eq!(a.files[0].listed, Some(MAX_LISTED));
        assert!(a.files[0].cut);
        assert!(a.message.contains("[cut: more files not listed after 200]"));
        std::os::unix::fs::symlink("/etc", dir.path().join("etc-link")).unwrap();
        let a = attach(dir.path(), "@etc-link");
        assert!(a.files.is_empty());
        assert!(a.warnings[0].contains("leads out"), "{:?}", a.warnings);
    }

    #[test]
    fn a_named_file_is_attached_after_the_text() {
        let dir = Scratch::new("attach-one");
        put(&dir, "notes/a.txt", "hello\nworld\n");
        let a = attach(dir.path(), "explain @notes/a.txt please");
        assert_eq!(
            a.message,
            "explain @notes/a.txt please\n\nAttached files:\n\n--- notes/a.txt ---\nhello\nworld\n--- end of notes/a.txt ---\n"
        );
        assert_eq!(
            a.files,
            vec![Attached {
                path: "notes/a.txt".into(),
                bytes: 12,
                cut: false,
                listed: None
            }]
        );
        assert!(a.warnings.is_empty());
    }

    #[test]
    fn two_files_in_order_a_repeat_once_and_missing_words_stay_plain() {
        let dir = Scratch::new("attach-many");
        put(&dir, "a.txt", "A");
        put(&dir, "b.txt", "B\n");
        let a = attach(
            dir.path(),
            "@b.txt then @a.txt, @b.txt and @nope.txt and me@a.txt and @/etc/passwd @../x",
        );
        let paths: Vec<&str> = a.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["b.txt", "a.txt"]);
        assert!(a.message.contains(
            "--- b.txt ---\nB\n--- end of b.txt ---\n--- a.txt ---\nA\n--- end of a.txt ---\n"
        ));
        assert!(a.warnings.is_empty(), "{:?}", a.warnings);
    }

    #[test]
    fn no_mention_leaves_the_message_unchanged() {
        let dir = Scratch::new("attach-none");
        let a = attach(dir.path(), "just text, no files @here");
        assert_eq!(a.message, "just text, no files @here");
        assert!(a.files.is_empty());
    }

    #[test]
    fn a_long_file_is_cut_with_a_note_at_a_character_boundary() {
        let dir = Scratch::new("attach-cut");
        // 3-byte characters: 20,000 is not a multiple of 3, so the cut lands inside one.
        put(&dir, "big.txt", &"€".repeat(7000));
        let a = attach(dir.path(), "@big.txt");
        assert_eq!(a.files.len(), 1);
        assert!(a.files[0].cut);
        assert_eq!(a.files[0].bytes, 21_000);
        let taken = 6666 * 3;
        assert!(
            a.message
                .contains(&format!("[cut: {} more bytes]", 21_000 - taken)),
            "{}",
            &a.message[a.message.len() - 80..]
        );
    }

    #[test]
    fn a_file_that_is_not_text_or_leads_out_is_not_attached() {
        let dir = Scratch::new("attach-bad");
        fs::write(dir.path().join("bin.dat"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        fs::write(dir.path().join("nul.txt"), "a\0b").unwrap();
        let outside = Scratch::new("attach-outside");
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("leak.txt"),
        )
        .unwrap();
        let a = attach(dir.path(), "@bin.dat @nul.txt @leak.txt");
        assert!(a.files.is_empty());
        assert_eq!(a.message, "@bin.dat @nul.txt @leak.txt");
        assert_eq!(a.warnings.len(), 3, "{:?}", a.warnings);
        assert!(a.warnings[0].contains("not UTF-8"));
        assert!(a.warnings[2].contains("leads out"));
    }

    #[test]
    fn a_message_is_limited_to_sixty_thousand_bytes_of_files() {
        let dir = Scratch::new("attach-total");
        for name in ["a", "b", "c", "d"] {
            put(&dir, &format!("{name}.txt"), &"x".repeat(20_000));
        }
        let a = attach(dir.path(), "@a.txt @b.txt @c.txt @d.txt");
        assert_eq!(a.files.len(), 3);
        assert_eq!(a.warnings.len(), 1);
        assert!(a.warnings[0].starts_with("@d.txt"));
    }
}
