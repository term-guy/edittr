//! Disk-backed index for the definition patterns used by the symbol pickers.
use crate::{goto, workspace};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const SCHEMA: u32 = 1;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 50_000;
const MAX_DEFINITIONS: usize = 1024;
pub(crate) const MAX_FILE_BYTES: u64 = 2_000_000;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Definition {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) line: usize,
    pub(crate) cols: Range<usize>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct Stamp {
    modified: SystemTime,
    bytes: u64,
}

impl Stamp {
    fn from_metadata(meta: &Metadata) -> Option<Self> {
        (meta.is_file() && meta.len() <= MAX_FILE_BYTES).then_some(())?;
        Some(Self {
            modified: meta.modified().ok()?,
            bytes: meta.len(),
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Entry {
    stamp: Stamp,
    digest: [u8; 32],
    definitions: Vec<Definition>,
}

#[derive(Deserialize, Serialize)]
struct Index {
    schema: u32,
    entries: BTreeMap<String, Entry>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Snapshot {
    index: Index,
    pub(crate) overflow: Vec<String>,
}

pub(crate) fn location(root: &Path) -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("edittr/symbols")
            .join(format!("{}.json", workspace::root_hash(root))),
    )
}

fn load(path: &Path) -> Option<Index> {
    let mut bytes = Vec::new();
    File::open(path).ok()?.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let index: Index = serde_json::from_slice(&bytes).ok()?;
    (index.schema == SCHEMA && index.entries.len() <= MAX_FILES).then_some(index)
}

pub(crate) fn source(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(path).ok()?.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES || bytes.contains(&0) {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    Some(if text.contains("\r\n") { text.replace("\r\n", "\n") } else { text })
}

impl Snapshot {
    pub(crate) fn refresh(
        root: &Path,
        cache: Option<&Path>,
        previous: Option<&Self>,
        files: Vec<String>,
        changed: &HashSet<PathBuf>,
        cancel: &AtomicBool,
    ) -> Self {
        let loaded = previous.is_none().then(|| cache.and_then(load)).flatten();
        let old = previous.map(|previous| &previous.index).or(loaded.as_ref());
        let mut snapshot = Self::default();
        let mut bytes = 100;
        let mut updated = false;
        for rel in files {
            if cancel.load(Ordering::Relaxed) {
                return snapshot;
            }
            if snapshot.index.entries.contains_key(&rel) {
                continue;
            }
            let path = root.join(&rel);
            let Ok(meta) = fs::metadata(&path) else { continue };
            if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                continue;
            }
            if snapshot.index.entries.len() == MAX_FILES || bytes >= MAX_BYTES {
                snapshot.overflow.push(rel);
                continue;
            }
            let Some(stamp) = Stamp::from_metadata(&meta) else {
                snapshot.overflow.push(rel);
                continue;
            };
            let cached = old
                .and_then(|old| old.entries.get(&rel))
                .filter(|entry| entry.stamp == stamp && !path.ancestors().any(|dir| changed.contains(dir)));
            let fresh;
            let entry = if let Some(cached) = cached {
                cached
            } else {
                updated = true;
                let Some(text) = source(&path) else { continue };
                let definitions: Vec<_> = goto::definitions(&text)
                    .take(MAX_DEFINITIONS + 1)
                    .map(|def| Definition {
                        name: def.name.to_owned(),
                        kind: def.kind.to_owned(),
                        line: def.line,
                        cols: def.cols,
                    })
                    .collect();
                if definitions.len() > MAX_DEFINITIONS
                    || fs::metadata(&path).ok().as_ref().and_then(Stamp::from_metadata).as_ref() != Some(&stamp)
                {
                    snapshot.overflow.push(rel);
                    continue;
                }
                fresh = Entry {
                    stamp,
                    digest: Sha256::digest(text.as_bytes()).into(),
                    definitions,
                };
                &fresh
            };
            // Upper bound for JSON strings (control characters can take six bytes)
            // plus the fields, punctuation, numeric offsets and content digest.
            let size = 300
                + 6 * rel.len()
                + entry
                    .definitions
                    .iter()
                    .map(|def| 140 + 6 * (def.name.len() + def.kind.len()))
                    .sum::<usize>();
            if bytes + size > MAX_BYTES {
                snapshot.overflow.push(rel);
                continue;
            }
            bytes += size;
            snapshot.index.entries.insert(rel, entry.clone());
        }
        if !cancel.load(Ordering::Relaxed)
            && let Some(path) = cache
            && (updated || old.is_none_or(|old| old.entries.len() != snapshot.index.entries.len()))
            && let Ok(json) = serde_json::to_string(&snapshot.index)
            && json.len() <= MAX_BYTES
        {
            let _ = workspace::write_config(path, &json);
        }
        snapshot
    }

    pub(crate) fn files(&self) -> impl Iterator<Item = (&str, &[Definition])> {
        self.index
            .entries
            .iter()
            .map(|(rel, entry)| (rel.as_str(), entry.definitions.as_slice()))
    }

    /// Cached definitions of one file, if `text` matches what was cached.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn document(&self, rel: &str, text: &str) -> Option<&[Definition]> {
        let entry = self.index.entries.get(rel)?;
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        (entry.digest == digest).then_some(&entry.definitions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reloads_and_invalidates_disk_symbols_by_stamp_and_watcher_paths() {
        let fixture = std::env::temp_dir().join(format!("edittr-symbol-cache-{}", std::process::id()));
        let root = fixture.join("project");
        let cache = fixture.join("symbols.json");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("lib.rs");
        let first = "fn first() {}\n";
        let later = "fn later() {}\n";
        let third = "fn third() {}\n";
        fs::write(&file, first).unwrap();
        let initial_mtime = fs::metadata(&file).unwrap().modified().unwrap();
        let refresh = |previous: Option<&Snapshot>, files, changed: &HashSet<PathBuf>| {
            Snapshot::refresh(&root, Some(&cache), previous, files, changed, &AtomicBool::new(false))
        };
        let files = || vec!["lib.rs".into()];
        let unchanged = HashSet::new();
        let initial = refresh(None, files(), &unchanged);
        assert_eq!(initial.document("lib.rs", first).unwrap()[0].name, "first");
        assert!(initial.document("lib.rs", later).is_none());
        assert!(cache.is_file());
        let reopened = refresh(None, files(), &unchanged);
        assert_eq!(reopened.document("lib.rs", first).unwrap()[0].name, "first");

        // Same size and timestamp. A watcher event must still invalidate the entry.
        fs::write(&file, later).unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(initial_mtime)
            .unwrap();
        let unchanged_on_disk = refresh(None, files(), &unchanged);
        assert_eq!(unchanged_on_disk.document("lib.rs", first).unwrap()[0].name, "first");
        let changed = HashSet::from([file.clone()]);
        let updated = refresh(Some(&reopened), files(), &changed);
        assert_eq!(updated.document("lib.rs", later).unwrap()[0].name, "later");

        // While closed, a timestamp change is enough even when the size matches.
        fs::write(&file, third).unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(initial_mtime + Duration::from_secs(10))
            .unwrap();
        let reopened = refresh(None, files(), &unchanged);
        assert_eq!(reopened.document("lib.rs", third).unwrap()[0].name, "third");

        // Same timestamp with a changed size still gets reparsed on reopen.
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        let text = "fn longer_name() {}\r\n";
        fs::write(&file, text).unwrap();
        File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        let reopened = refresh(None, files(), &unchanged);
        assert_eq!(
            reopened.document("lib.rs", &text.replace("\r\n", "\n")).unwrap()[0].name,
            "longer_name"
        );

        fs::remove_file(file).unwrap();
        assert_eq!(refresh(Some(&reopened), files(), &changed).files().count(), 0);
        assert_eq!(refresh(None, files(), &unchanged).files().count(), 0);
        fs::remove_dir_all(fixture).unwrap();
    }
}
