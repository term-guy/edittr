//! Byte-oriented file access and offset-aligned comparisons. I/O runs on workers.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

pub(crate) const ROW_BYTES: usize = 16;
const CHUNK: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct Source {
    pub size: u64,
    file: Option<Arc<Backing>>,
}

struct Backing {
    file: Mutex<File>,
    // Dropped after the file handle closes, including on Windows.
    _temp: Option<TemporaryFile>,
}

struct TemporaryFile(PathBuf);

impl TemporaryFile {
    fn create() -> io::Result<(Self, File)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        loop {
            let path = std::env::temp_dir().join(format!("edittr-hex-{}-{}", std::process::id(), NEXT.fetch_add(1, Relaxed)));
            match options.open(&path) {
                Ok(file) => return Ok((Self(path), file)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.0);
        std::thread::spawn(move || {
            let _ = std::fs::remove_file(path);
        });
    }
}

impl Source {
    pub(crate) fn empty() -> Self {
        Self { size: 0, file: None }
    }

    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::Error::other("Expected a regular file"));
        }
        Ok(Self::from_file(File::open(path)?, meta.len()))
    }

    pub(crate) fn from_file(file: File, size: u64) -> Self {
        Self {
            size,
            file: Some(Arc::new(Backing {
                file: Mutex::new(file),
                _temp: None,
            })),
        }
    }

    /// Spool show/cat output to disk so a repository blob needs no file-sized allocation.
    pub(crate) fn from_command(mut command: Command) -> Result<Self, String> {
        let (temp, file) = TemporaryFile::create().map_err(|err| err.to_string())?;
        let output = command
            .stdout(Stdio::from(file.try_clone().map_err(|err| err.to_string())?))
            .output()
            .map_err(|err| err.to_string())?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            return Err(if error.trim().is_empty() {
                output.status.to_string()
            } else {
                error.trim().to_owned()
            });
        }
        let size = file.metadata().map_err(|err| err.to_string())?.len();
        Ok(Self {
            size,
            file: Some(Arc::new(Backing {
                file: Mutex::new(file),
                _temp: Some(temp),
            })),
        })
    }

    #[cfg(any(test, feature = "tour"))]
    pub(crate) fn fixture(bytes: &[u8]) -> io::Result<Self> {
        use std::io::Write as _;
        let (temp, mut file) = TemporaryFile::create()?;
        file.write_all(bytes)?;
        Ok(Self {
            size: bytes.len() as u64,
            file: Some(Arc::new(Backing {
                file: Mutex::new(file),
                _temp: Some(temp),
            })),
        })
    }

    pub(crate) fn is_binary(&self) -> io::Result<bool> {
        Ok(is_binary(&self.read(0..8000)?))
    }

    pub(crate) fn read(&self, range: Range<u64>) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.read_into(range, &mut buf)?;
        Ok(buf)
    }

    fn read_into(&self, range: Range<u64>, buf: &mut Vec<u8>) -> io::Result<()> {
        let end = range.end.min(self.size);
        let len = usize::try_from(end.saturating_sub(range.start)).map_err(io::Error::other)?;
        buf.clear();
        if len != 0
            && let Some(backing) = &self.file
        {
            let mut file = backing.file.lock().unwrap();
            file.seek(SeekFrom::Start(range.start))?;
            buf.try_reserve(len).map_err(io::Error::other)?;
            buf.resize(len, 0);
            file.read_exact(buf)?;
        }
        Ok(())
    }
}

pub(crate) fn is_binary(sample: &[u8]) -> bool {
    let invalid = std::str::from_utf8(sample).is_err_and(|err| err.error_len().is_some());
    (invalid && sample.iter().filter(|b| !b.is_ascii()).count() > sample.len() / 5)
        || sample.contains(&0)
        || sample.iter().filter(|&&b| matches!(b, 1..=8 | 14..=26 | 28..=31)).count() > sample.len() / 10
}

pub(crate) fn parse_offset(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => u64::from_str_radix(hex, 16).ok(),
        None if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => text.parse().ok(),
        _ => None,
    }
}

pub(crate) fn hex_string(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().saturating_mul(3));
    for (i, byte) in bytes.iter().enumerate() {
        if i != 0 {
            text.push(' ');
        }
        let _ = write!(text, "{byte:02X}");
    }
    text
}

pub(crate) fn ascii(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| if matches!(b, b' '..=b'~') { char::from(b) } else { '.' })
        .collect()
}

pub(crate) fn diff_ranges(left: &[u8], right: &[u8]) -> impl Iterator<Item = Range<usize>> {
    let end = left.len().max(right.len());
    let differs = move |i| left.get(i) != right.get(i);
    let mut cursor = 0;
    std::iter::from_fn(move || {
        let start = (cursor..end).find(|&i| differs(i))?;
        cursor = (start + 1..end).find(|&i| !differs(i)).unwrap_or(end);
        Some(start..cursor)
    })
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Summary {
    pub count: u64,
    pub first: Option<u64>,
    pub last: Option<u64>,
}

/// Find contiguous difference runs without storing a byte-sized comparison index.
#[derive(Clone)]
pub(crate) struct Pair {
    pub left: Source,
    pub right: Source,
}

impl Pair {
    pub(crate) fn size(&self) -> u64 {
        self.left.size.max(self.right.size)
    }

    pub(crate) fn summarize(&self, cancelled: &AtomicBool) -> io::Result<Summary> {
        let mut summary = Summary::default();
        let mut buffers = (Vec::new(), Vec::new());
        for offset in (0..self.size()).step_by(CHUNK) {
            if cancelled.load(Relaxed) {
                break;
            }
            self.read_chunk(&mut buffers, offset, offset.saturating_add(CHUNK as u64))?;
            if buffers.0 == buffers.1 {
                continue;
            }
            for range in diff_ranges(&buffers.0, &buffers.1) {
                summary.count += range.len() as u64;
                summary.first.get_or_insert(offset + range.start as u64);
                summary.last = Some(offset + range.end as u64 - 1);
            }
        }
        Ok(summary)
    }

    fn read_chunk(&self, buffers: &mut (Vec<u8>, Vec<u8>), start: u64, end: u64) -> io::Result<()> {
        self.left.read_into(start..end, &mut buffers.0)?;
        self.right.read_into(start..end, &mut buffers.1)
    }

    pub(crate) fn navigate(&self, from: u64, backwards: bool, cancelled: &AtomicBool) -> io::Result<Option<u64>> {
        let mut buffers = (Vec::new(), Vec::new());
        let mut offset = from.min(self.size());
        let mut phase = SeekDiff::Initial;
        let mut found = None;
        loop {
            if cancelled.load(Relaxed) {
                return Ok(None);
            }
            let (start, end) = if backwards {
                let end = offset.saturating_add(1).min(self.size());
                (end.saturating_sub(CHUNK as u64), end)
            } else {
                (offset, offset.saturating_add(CHUNK as u64).min(self.size()))
            };
            self.read_chunk(&mut buffers, start, end)?;
            let mut inspect = |i: usize| {
                let different = buffers.0.get(i) != buffers.1.get(i);
                phase = match (phase, different) {
                    (SeekDiff::Initial, true) => SeekDiff::SkipRun,
                    (SeekDiff::Initial | SeekDiff::SkipRun, false) => SeekDiff::Find,
                    (SeekDiff::Find | SeekDiff::Found, true) => {
                        found = Some(start + i as u64);
                        SeekDiff::Found
                    }
                    (SeekDiff::Found, false) => return true,
                    (phase, _) => phase,
                };
                matches!(phase, SeekDiff::Found) && !backwards
            };
            let count = (end - start) as usize;
            if backwards {
                if (0..count).rev().any(inspect) || start == 0 {
                    return Ok(found);
                }
                offset = start - 1;
            } else {
                if (0..count).any(&mut inspect) || end == self.size() {
                    return Ok(found);
                }
                offset = end;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SeekDiff {
    Initial,
    SkipRun,
    Find,
    Found,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_partial_rows_and_clipboard_data() {
        let bytes = [0, b' ', b'A', 0x7e, 0x7f, 0xff];
        assert_eq!(hex_string(&bytes), "00 20 41 7E 7F FF");
        assert_eq!(ascii(&bytes), ". A~..");
        assert_eq!(hex_string(&[]), "");
    }

    #[test]
    fn finds_difference_ranges_including_a_missing_tail() {
        assert_eq!(diff_ranges(b"abcdefg", b"axbdez").collect::<Vec<_>>(), [1..3, 5..7]);
        assert_eq!(diff_ranges(b"", b"abc").next(), Some(0..3));
        assert_eq!(diff_ranges(b"abc", b"abc").next(), None);
    }

    #[test]
    fn scans_and_navigates_across_chunks_and_unequal_lengths() {
        let mut left = vec![0; CHUNK * 2];
        let mut right = left.clone();
        let boundary = CHUNK - 2;
        left[3..7].fill(1);
        right[boundary..boundary + 4].fill(2);
        right.push(3);
        let pair = Pair {
            left: Source::fixture(&left).unwrap(),
            right: Source::fixture(&right).unwrap(),
        };
        let cancel = AtomicBool::new(false);
        let summary = pair.summarize(&cancel).unwrap();
        let boundary = boundary as u64;
        let last = (CHUNK * 2) as u64;
        assert_eq!((summary.count, summary.first, summary.last), (9, Some(3), Some(last)));
        assert_eq!(pair.navigate(0, false, &cancel).unwrap(), Some(3));
        assert_eq!(pair.navigate(3, false, &cancel).unwrap(), Some(boundary));
        assert_eq!(pair.navigate(boundary + 3, false, &cancel).unwrap(), Some(last));
        assert_eq!(pair.navigate(last, true, &cancel).unwrap(), Some(boundary));
        assert_eq!(pair.navigate(boundary, true, &cancel).unwrap(), Some(3));
        assert_eq!(pair.navigate(3, true, &cancel).unwrap(), None);
    }

    #[test]
    fn opens_a_sparse_gigabyte_binary_and_seeks_to_the_tail() {
        use std::io::Write as _;
        let (temp, mut file) = TemporaryFile::create().unwrap();
        let len = 1 << 30;
        file.set_len(len).unwrap();
        file.seek(SeekFrom::End(-3)).unwrap();
        file.write_all(&[1, 2, 3]).unwrap();
        drop(file);
        let source = Source::open(&temp.0).unwrap();
        assert_eq!(source.size, len);
        assert!(source.is_binary().unwrap());
        assert_eq!(source.read(len - 4..len + 8).unwrap(), [0, 1, 2, 3]);
    }

    #[test]
    fn parses_decimal_and_hex_offsets() {
        assert_eq!(parse_offset(" 0x1A2B "), Some(0x1a2b));
        assert_eq!(parse_offset("0Xff"), Some(255));
        assert_eq!(parse_offset("00255"), Some(255));
        assert_eq!(parse_offset("0"), Some(0));
        for invalid in ["", "0x", "0xno", "-1", "+4", "18446744073709551616"] {
            assert_eq!(parse_offset(invalid), None);
        }
    }
}
