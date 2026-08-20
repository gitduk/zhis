//! JSONL history store: entries are appended as JSON lines, reads and writes
//! are protected by flock, and entry IDs are `offset-base36` + a persistent
//! sequence number so a `get` can seek directly instead of scanning the file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Largest single JSON line we will read or write, in bytes.
const MAX_JSON_LINE_SIZE: usize = 4 * 1024 * 1024;

/// Buffer size for line-oriented file reads (`read_all`, `import_history`).
pub(crate) const READ_BUF_CAP: usize = 64 * 1024;

/// A single stored entry (`{"t":..,"d":..,"x":..,"c":..,"m":..}`, `m` omitted
/// when 0). Identity lives on `StoredLine`, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(rename = "t")]
    pub t: i64,
    #[serde(rename = "d")]
    pub d: String,
    #[serde(rename = "x")]
    pub x: i64,
    #[serde(rename = "c")]
    pub c: String,
    #[serde(rename = "m", skip_serializing_if = "is_zero", default)]
    pub m: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// The mode with group and other stripped, or None when it is already
/// owner-only. Both stores hold pasted secrets, so both mask the same bits.
pub(crate) fn owner_only(mode: u32) -> Option<u32> {
    (mode & 0o077 != 0).then_some(mode & !0o077)
}

/// On-disk line, read side. Not `#[serde(flatten)]` over `Entry` — that
/// forces serde_json's slower buffered path on every row.
#[derive(Deserialize)]
struct StoredLine {
    #[serde(rename = "i", default)]
    i: i64,
    #[serde(rename = "t")]
    t: i64,
    #[serde(rename = "d")]
    d: String,
    #[serde(rename = "x")]
    x: i64,
    #[serde(rename = "c")]
    c: String,
    #[serde(rename = "m", default)]
    m: i64,
}

impl StoredLine {
    fn into_parts(self) -> (i64, Entry) {
        let StoredLine { i, t, d, x, c, m } = self;
        (i, Entry { t, d, x, c, m })
    }
}

/// Write side of the same line, borrowing `Entry`'s fields instead of
/// cloning them — `write_all` re-encodes every surviving row on a delete.
#[derive(Serialize)]
struct StoredLineRef<'a> {
    #[serde(rename = "i")]
    i: i64,
    #[serde(rename = "t")]
    t: i64,
    #[serde(rename = "d")]
    d: &'a str,
    #[serde(rename = "x")]
    x: i64,
    #[serde(rename = "c")]
    c: &'a str,
    #[serde(rename = "m", skip_serializing_if = "is_zero")]
    m: i64,
}

impl<'a> StoredLineRef<'a> {
    fn new(i: i64, entry: &'a Entry) -> Self {
        let Entry { t, d, x, c, m } = entry;
        StoredLineRef {
            i,
            t: *t,
            d,
            x: *x,
            c,
            m: *m,
        }
    }
}

/// An entry plus the ID the fzf picker sees, and how many consecutive
/// repeats it stands for (1 outside the dedupe view).
#[derive(Debug, Clone)]
pub struct Row {
    pub entry: Entry,
    pub id: String,
    pub count: usize,
}

/// What the picker asks for. `None` means unrestricted; `uniq` collapses
/// consecutive repeats to their newest entry.
#[derive(Debug, Default, Clone)]
pub struct Query {
    pub dir: Option<String>,
    pub limit: Option<usize>,
    pub uniq: bool,
}

struct StoredRow {
    entry: Entry,
    id: String,
    offset: i64,
    i: i64,
    count: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("entry not found")]
    EntryNotFound,
    #[error("{0}")]
    Message(String),
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Store { path }
    }

    /// Appends entries under an exclusive lock, each assigned a fresh
    /// sequence number; rolls the data file back to its original size on failure.
    pub fn append(&self, entries: &[Entry]) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }

        self.with_lock(LockMode::Ex, || {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .mode(0o600)
                .open(&self.path)?;
            let meta = f.metadata()?;
            let original_size = meta.len() as i64;
            // `.mode()` applies only when creating; an existing file keeps its
            // own bits.
            if let Some(m) = owner_only(meta.permissions().mode()) {
                f.set_permissions(fs::Permissions::from_mode(m))?;
            }

            let start = self.read_seq(&mut f, original_size)?;
            let mut encoded = Vec::with_capacity(entries.len());
            for (idx, entry) in entries.iter().enumerate() {
                encoded.push(encode_line(start + 1 + idx as i64, entry)?);
            }
            // Deliberately before the data write: a rolled-back append leaves
            // a gap in the sequence, which is harmless, but a reused id is not.
            self.write_seq(start + entries.len() as i64)?;

            let result = (|| -> Result<(), StoreError> {
                if original_size > 0 {
                    // content_end == size means the file does not end in a
                    // newline, so this append would glue onto the last line.
                    if content_end(&mut f, original_size)? == original_size {
                        let trailing = trailing_line_size(&mut f, original_size)?;
                        if trailing >= MAX_JSON_LINE_SIZE as i64 {
                            return Err(StoreError::Message(format!(
                                "cannot append newline to trailing line of at least {} bytes; limit is {}",
                                trailing, MAX_JSON_LINE_SIZE
                            )));
                        }
                        f.write_all(b"\n")?;
                    }
                }
                for line in &encoded {
                    f.write_all(line)?;
                }
                Ok(())
            })();
            if result.is_err() {
                // Roll the file back to its original size; a partially written
                // line must never be left behind.
                if f.set_len(original_size as u64).is_err() {
                    let _ = OpenOptions::new().write(true).open(&self.path)
                        .and_then(|g| g.set_len(original_size as u64));
                }
            }
            result
        })
    }

    /// Every row in file order, locked. Only tests need this shape — they
    /// assert on stored layout, where `query`'s time ordering would obscure it.
    #[cfg(test)]
    pub fn list(&self) -> Result<Vec<Row>, StoreError> {
        let rows = self.with_lock(LockMode::Sh, || self.read_all())?;
        Ok(rows
            .into_iter()
            .map(|r| Row {
                entry: r.entry,
                id: r.id,
                count: r.count,
            })
            .collect())
    }

    /// Rows for the picker, newest first. A limit selects by file position and
    /// only then sorts by time; they differ only inside imported history.
    pub fn query(&self, q: &Query) -> Result<Vec<Row>, StoreError> {
        let dir = q.dir.as_deref();
        // `read_tail` with `uniq` already returns rows sorted newest-first and
        // folded; every other path returns file-order rows needing both.
        let presorted = q.uniq && q.limit.is_some();
        let mut rows = self.with_lock(LockMode::Sh, || match q.limit {
            None => self.read_rows(0, dir, None),
            Some(want) => self.read_tail(want, dir, q.uniq),
        })?;
        if !presorted {
            // SHARE_HISTORY-imported files interleave sessions, so file order
            // is not time order. Stable, so same-timestamp rows keep order.
            rows.sort_by_key(|r| r.entry.t);
            rows.reverse();
            if q.uniq {
                fold_runs(&mut rows);
            }
        }
        if let Some(want) = q.limit {
            rows.truncate(want);
        }
        Ok(rows
            .into_iter()
            .map(|r| Row {
                entry: r.entry,
                id: r.id,
                count: r.count,
            })
            .collect())
    }

    /// Reads back far enough to yield `want` matching rows, widening the
    /// window for a sparse `dir` filter or, with `uniq`, for folded shrinkage.
    fn read_tail(
        &self,
        want: usize,
        dir: Option<&str>,
        uniq: bool,
    ) -> Result<Vec<StoredRow>, StoreError> {
        if want == 0 {
            return Ok(Vec::new());
        }
        let mut f = match self.open_data()? {
            Some(f) => f,
            None => return Ok(Vec::new()),
        };
        let size = f.metadata()?.len() as i64;

        let mut window = want;
        loop {
            let (from, hit_start) = nth_line_start_from_end(&mut f, size, window)?;
            let rows = self.read_window(&mut f, from, dir, want, uniq)?;
            if rows.len() >= want || hit_start {
                return Ok(rows);
            }
            // Quadrupling keeps the re-read count logarithmic; the discarded
            // work stays a constant factor of what the answer costs.
            window = match window.checked_mul(4) {
                Some(w) => w,
                None => return self.read_window(&mut f, 0, dir, want, uniq),
            };
        }
    }

    /// Reads `from`..EOF filtered by `dir`; with `uniq`, sorts newest-first
    /// and folds consecutive repeats, counting each run.
    fn read_window(
        &self,
        f: &mut File,
        from: i64,
        dir: Option<&str>,
        want: usize,
        uniq: bool,
    ) -> Result<Vec<StoredRow>, StoreError> {
        let mut rows = self.rows_from(f, from, dir, if uniq { None } else { Some(want) })?;
        if uniq {
            rows.sort_by_key(|r| r.entry.t);
            rows.reverse();
            fold_runs(&mut rows);
        }
        Ok(rows)
    }

    pub fn get(&self, id: &str) -> Result<Entry, StoreError> {
        let (offset, i) = match parse_id(id) {
            Some(v) => v,
            None => return Err(StoreError::EntryNotFound),
        };
        // Fast path: seek straight to the recorded offset without any lock.
        // Verification rejects torn reads before the locked fallback below.
        if let Some(entry) = self.get_at(offset, i)? {
            return Ok(entry);
        }

        let rows = self.with_lock(LockMode::Sh, || self.read_all())?;
        match find_by_seq(&rows, i) {
            Some(idx) => Ok(rows[idx].entry.clone()),
            None => Err(StoreError::EntryNotFound),
        }
    }

    /// Deletes an entry by ID, or every entry with the same command when
    /// `all` is set. Unknown IDs are a no-op.
    pub fn delete(&self, id: &str, all: bool) -> Result<(), StoreError> {
        let (offset, i) = match parse_id(id) {
            Some(v) => v,
            None => return Ok(()),
        };

        self.with_lock(LockMode::Ex, || {
            let rows = self.read_all()?;
            // Both arms name the same row (i is unique); the first only
            // avoids a reverse scan while the offset is still current.
            let target = rows
                .iter()
                .position(|row| row.offset == offset && row.i == i)
                .or_else(|| find_by_seq(&rows, i));
            let target = match target {
                Some(t) => t,
                None => return Ok(()),
            };

            let target_cmd = rows[target].entry.c.clone();
            let mut kept: Vec<(i64, Entry)> = Vec::with_capacity(rows.len().saturating_sub(1));
            for (idx, row) in rows.into_iter().enumerate() {
                if all {
                    if row.entry.c != target_cmd {
                        kept.push((row.i, row.entry));
                    }
                    continue;
                }
                if idx != target {
                    kept.push((row.i, row.entry));
                }
            }
            self.write_all(&kept)
        })
    }

    /// Direct seek-read of the line at `offset`, verifying its sequence
    /// number. Returns Ok(None) when the read is absent or doesn't match.
    fn get_at(&self, offset: i64, want_i: i64) -> Result<Option<Entry>, StoreError> {
        let mut f = match self.open_data()? {
            Some(f) => f,
            None => return Ok(None),
        };
        f.seek(SeekFrom::Start(offset as u64))?;

        let mut line = Vec::new();
        BufReader::new(f.take((MAX_JSON_LINE_SIZE + 1) as u64)).read_until(b'\n', &mut line)?;
        if line.is_empty() || line.len() > MAX_JSON_LINE_SIZE {
            return Ok(None);
        }
        let (i, entry) = match decode_line(&line) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if entry.c.is_empty() || i != want_i {
            return Ok(None);
        }
        Ok(Some(entry))
    }

    /// Reads every row. Callers must hold a lock; `delete` holds the exclusive
    /// lock and relocking it from within would deadlock.
    fn read_all(&self) -> Result<Vec<StoredRow>, StoreError> {
        self.read_rows(0, None, None)
    }

    /// Reads `from`..EOF; `from` MUST be a line start or every line is garbage.
    /// Filtering here, not in the caller, lets `query` stop short of the file.
    fn read_rows(
        &self,
        from: i64,
        dir: Option<&str>,
        keep_last: Option<usize>,
    ) -> Result<Vec<StoredRow>, StoreError> {
        let mut f = match self.open_data()? {
            Some(f) => f,
            None => return Ok(Vec::new()),
        };
        self.rows_from(&mut f, from, dir, keep_last)
    }

    /// `read_rows` against a handle the caller already has, so the limited
    /// path opens the file once instead of once per widening pass.
    fn rows_from(
        &self,
        f: &mut File,
        from: i64,
        dir: Option<&str>,
        keep_last: Option<usize>,
    ) -> Result<Vec<StoredRow>, StoreError> {
        f.seek(SeekFrom::Start(from as u64))?;

        let mut reader = BufReader::with_capacity(READ_BUF_CAP, f);
        let mut rows: Vec<StoredRow> = Vec::new();
        // Absolute, not relative to `from`: ids embed this offset and `get`
        // seeks straight to it.
        let mut offset: i64 = from;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            let at = || format!("read {} at offset {}", self.path.display(), offset);
            if line.len() > MAX_JSON_LINE_SIZE {
                return Err(StoreError::Message(format!(
                    "{}: encoded JSON line is {} bytes; limit is {}",
                    at(),
                    line.len(),
                    MAX_JSON_LINE_SIZE
                )));
            }
            let (i, entry) =
                decode_line(&line).map_err(|e| StoreError::Message(format!("{}: {}", at(), e)))?;
            if entry.c.is_empty() {
                return Err(StoreError::Message(format!("{}: empty command", at())));
            }
            if dir.is_none_or(|d| entry.d == d) {
                rows.push(StoredRow {
                    id: make_id(offset, i),
                    entry,
                    offset,
                    i,
                    count: 1,
                });
            }
            offset += line.len() as i64;
        }
        // The window `query` picked can overshoot; the newest are wanted.
        if let Some(n) = keep_last {
            if rows.len() > n {
                rows.drain(..rows.len() - n);
            }
        }
        Ok(rows)
    }

    /// Atomically rewrites the whole file: write a temp file, fsync it, rename
    /// it over the real path, then fsync the directory. Each row keeps the
    /// sequence number it already had — a rewrite must never reassign ids.
    fn write_all(&self, rows: &[(i64, Entry)]) -> Result<(), StoreError> {
        let tmp_path = PathBuf::from(format!("{}.tmp", self.path.display()));
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;

        let result = (|| -> Result<(), StoreError> {
            {
                let mut w = BufWriter::new(&mut f);
                for (i, entry) in rows {
                    w.write_all(&encode_line(*i, entry)?)?;
                }
                w.flush()?;
            }
            // Renaming without syncing the file and directory can expose empty
            // or stale history after a crash.
            f.sync_all()?;
            Ok(())
        })();

        if let Err(e) = result {
            drop(f);
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        drop(f);
        if let Err(e) = fs::rename(&tmp_path, &self.path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.into());
        }

        let dir = self.path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(dir) = dir {
            let d = File::open(dir)?;
            d.sync_all()?;
        }
        Ok(())
    }

    /// Opens the data file, or None when it does not exist. An absent store
    /// reads as empty everywhere, never as an error.
    fn open_data(&self) -> Result<Option<File>, StoreError> {
        match File::open(&self.path) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn seq_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.seq", self.path.display()))
    }

    /// Last sequence number assigned. Missing/corrupt/negative sidecar
    /// recovers from `f`'s last line instead of reissuing ids in use.
    fn read_seq(&self, f: &mut File, size: i64) -> Result<i64, StoreError> {
        match fs::read_to_string(self.seq_path()) {
            Ok(s) => {
                if let Ok(n) = s.trim().parse::<i64>() {
                    if n >= 0 {
                        return Ok(n);
                    }
                }
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            Err(_) => {}
        }
        match read_last_line(f, size)? {
            None => Ok(0),
            Some(line) => {
                let (i, _) = decode_line(&line).map_err(|e| {
                    StoreError::Message(format!(
                        "recovering sequence counter from {}: {}",
                        self.path.display(),
                        e
                    ))
                })?;
                if i < 0 {
                    return Err(StoreError::Message(format!(
                        "recovered a negative sequence number from {}",
                        self.path.display()
                    )));
                }
                Ok(i)
            }
        }
    }

    fn write_seq(&self, next: i64) -> Result<(), StoreError> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.seq_path())?;
        f.write_all(next.to_string().as_bytes())?;
        Ok(())
    }

    fn with_lock<T>(
        &self,
        mode: LockMode,
        f: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let lock_path = PathBuf::from(format!("{}.lock", self.path.display()));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)?;
        match mode {
            LockMode::Sh => lock.lock_shared()?,
            LockMode::Ex => lock.lock()?,
        }
        let result = f();
        let _ = lock.unlock();
        result
    }
}

#[derive(Clone, Copy)]
enum LockMode {
    Sh,
    Ex,
}

/// Size in bytes of the final line (not counting its trailing newline), or of
/// the whole file when it has no newline at all.
fn trailing_line_size(f: &mut File, size: i64) -> io::Result<i64> {
    const CHUNK: usize = 32 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let mut total: i64 = 0;
    let mut end = size;
    while end > 0 {
        let start = (end - CHUNK as i64).max(0);
        let n = (end - start) as usize;
        f.seek(SeekFrom::Start(start as u64))?;
        f.read_exact(&mut buf[..n])?;
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            return Ok(total + (n - i - 1) as i64);
        }
        total += n as i64;
        if total >= MAX_JSON_LINE_SIZE as i64 {
            return Ok(total);
        }
        end = start;
    }
    Ok(total)
}

/// End of the file's content, i.e. `size` minus a single trailing newline.
/// A file not ending in one still ends a line at `size`.
fn content_end(f: &mut File, size: i64) -> io::Result<i64> {
    if size == 0 {
        return Ok(0);
    }
    let mut last = [0u8; 1];
    f.seek(SeekFrom::Start((size - 1) as u64))?;
    f.read_exact(&mut last)?;
    Ok(if last[0] == b'\n' { size - 1 } else { size })
}

/// Start offset of the `n`-th line counting back from the end (`n` = 1 is the
/// last line), plus whether the scan ran out of file before finding `n` lines.
fn nth_line_start_from_end(f: &mut File, size: i64, n: usize) -> io::Result<(i64, bool)> {
    let end = content_end(f, size)?;
    if end == 0 || n == 0 {
        return Ok((0, true));
    }

    const CHUNK: i64 = 32 * 1024;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut seen = 0usize;
    let mut scan = end;
    while scan > 0 {
        let start = (scan - CHUNK).max(0);
        let len = (scan - start) as usize;
        f.seek(SeekFrom::Start(start as u64))?;
        f.read_exact(&mut buf[..len])?;
        let mut cur = len;
        while let Some(i) = buf[..cur].iter().rposition(|&b| b == b'\n') {
            seen += 1;
            if seen == n {
                return Ok((start + i as i64 + 1, false));
            }
            cur = i;
        }
        scan = start;
    }
    Ok((0, true))
}

/// Last complete line (trailing newline stripped), or None if empty. Only
/// used for `.seq` recovery — its backward scan never runs on the hot path.
fn read_last_line(f: &mut File, size: i64) -> io::Result<Option<Vec<u8>>> {
    if size == 0 {
        return Ok(None);
    }
    let (line_start, _) = nth_line_start_from_end(f, size, 1)?;
    let mut line = vec![0u8; (size - line_start) as usize];
    f.seek(SeekFrom::Start(line_start as u64))?;
    f.read_exact(&mut line)?;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(Some(line))
}

/// Folds each run of same-command rows into its newest, counting the run.
/// The caller must pass rows sorted newest-first.
fn fold_runs(rows: &mut Vec<StoredRow>) {
    if rows.len() <= 1 {
        return;
    }
    let mut w = 0;
    for r in 1..rows.len() {
        if rows[w].entry.c == rows[r].entry.c {
            rows[w].count += 1;
        } else {
            w += 1;
            if w != r {
                rows.swap(w, r);
            }
        }
    }
    rows.truncate(w + 1);
}

fn encode_line(i: i64, entry: &Entry) -> Result<Vec<u8>, StoreError> {
    if entry.c.is_empty() {
        return Err(StoreError::Message("empty command".to_string()));
    }
    let mut encoded = serde_json::to_vec(&StoredLineRef::new(i, entry))?;
    encoded.push(b'\n');
    if encoded.len() > MAX_JSON_LINE_SIZE {
        return Err(StoreError::Message(format!(
            "encoded JSON line for entry {} is {} bytes; limit is {}",
            i,
            encoded.len(),
            MAX_JSON_LINE_SIZE
        )));
    }
    Ok(encoded)
}

fn decode_line(bytes: &[u8]) -> Result<(i64, Entry), serde_json::Error> {
    let line: StoredLine = serde_json::from_slice(bytes)?;
    Ok(line.into_parts())
}

fn make_id(offset: i64, i: i64) -> String {
    format!("{}-{}", radix36(offset), radix36(i))
}

/// Index of the newest row with sequence number `i`. Only reached when the
/// offset in an ID has gone stale (a delete shifted it); `i` never changes,
/// so it's the only thing left to search on.
fn find_by_seq(rows: &[StoredRow], i: i64) -> Option<usize> {
    rows.iter().rposition(|row| row.i == i)
}

fn radix36(mut n: i64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    buf.into_iter().map(char::from).collect()
}

/// Parses `offset36-i36` into its parts. Returns None for malformed IDs.
fn parse_id(id: &str) -> Option<(i64, i64)> {
    let (offset_text, i_text) = id.split_once('-')?;
    if offset_text.is_empty() || i_text.is_empty() {
        return None;
    }
    let offset = i64::from_str_radix(offset_text, 36).ok()?;
    let i = i64::from_str_radix(i_text, 36).ok()?;
    if offset < 0 || i < 0 {
        return None;
    }
    Some((offset, i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_ids_and_inflight_ids_cannot_be_confused() {
        assert!(parse_id("2f-19").is_some());
        // `cmd_get` and `cmd_delete` both branch on this before reaching the
        // store: an in-flight id must never parse as a stored row.
        assert_eq!(parse_id(&crate::inflight::make_id(1234)), None);
        assert_eq!(parse_id("@1234"), None);
    }

    /// A `Store` in a temp dir that is removed on drop, panic included — the
    /// hand-rolled cleanup this replaced never ran on a failing assert.
    struct TestStore {
        _dir: tempfile::TempDir,
        store: Store,
    }

    impl std::ops::Deref for TestStore {
        type Target = Store;
        fn deref(&self) -> &Store {
            &self.store
        }
    }

    fn test_store() -> TestStore {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("history.jsonl"));
        TestStore { _dir: dir, store }
    }

    #[test]
    fn append_list_round_trip() {
        let store = test_store();
        let entries = vec![
            Entry {
                t: 10,
                d: "/tmp/one".into(),
                x: 0,
                c: "printf hello".into(),
                m: 1250,
            },
            Entry {
                t: 20,
                d: "/tmp/two".into(),
                x: 1,
                c: "printf 'one\\ntwo'\nprintf done".into(),
                m: 0,
            },
        ];
        store.append(&entries).unwrap();

        let rows = store.list().unwrap();
        assert_eq!(rows.len(), entries.len());
        for (row, entry) in rows.iter().zip(&entries) {
            assert_eq!(&row.entry, entry);
            assert!(
                !row.id.is_empty() && !row.id.chars().any(|c| c == ' ' || c == '\t' || c == '\n')
            );
        }
    }

    #[test]
    fn append_adds_missing_newline() {
        let store = test_store();
        let first = Entry {
            t: 10,
            d: "/tmp".into(),
            x: 0,
            c: "first".into(),
            m: 0,
        };
        let encoded = serde_json::to_vec(&first).unwrap();
        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        fs::write(&store.path, &encoded).unwrap();

        let second = Entry {
            t: 20,
            d: "/tmp".into(),
            x: 0,
            c: "second".into(),
            m: 0,
        };
        store.append(&[second]).unwrap();

        let data = fs::read(&store.path).unwrap();
        assert!(
            data.windows(2).any(|w| w[0] == b'}' && w[1] == b'\n'),
            "entries are not separated by a newline: {:?}",
            String::from_utf8_lossy(&data)
        );
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry, first);
        assert_eq!(rows[1].entry.c, "second");
    }

    #[test]
    fn rejects_newline_that_would_oversize_trailing_line() {
        let store = test_store();
        let base = serde_json::to_vec(&Entry {
            t: 10,
            d: "/tmp".into(),
            x: 0,
            c: String::new(),
            m: 0,
        })
        .unwrap();
        let entry = Entry {
            t: 10,
            d: "/tmp".into(),
            x: 0,
            c: "x".repeat(MAX_JSON_LINE_SIZE - base.len()),
            m: 0,
        };
        let encoded = serde_json::to_vec(&entry).unwrap();
        assert_eq!(encoded.len(), MAX_JSON_LINE_SIZE);
        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        fs::write(&store.path, &encoded).unwrap();

        let next = Entry {
            t: 20,
            c: "next".into(),
            d: String::new(),
            x: 0,
            m: 0,
        };
        assert!(store.append(&[next]).is_err());
        let after = fs::read(&store.path).unwrap();
        assert_eq!(after, encoded);
    }

    #[test]
    fn rejects_oversized_entry_without_changing_file() {
        let store = test_store();
        let seed = Entry {
            t: 10,
            d: "/tmp".into(),
            x: 0,
            c: "seed".into(),
            m: 0,
        };
        store.append(&[seed]).unwrap();
        let before = fs::read(&store.path).unwrap();

        let oversized = Entry {
            t: 20,
            d: "/tmp".into(),
            x: 0,
            c: "x".repeat(MAX_JSON_LINE_SIZE),
            m: 0,
        };
        assert!(store.append(&[oversized]).is_err());
        let after = fs::read(&store.path).unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn get_direct_does_not_scan_later_rows() {
        let store = test_store();
        let entry = Entry {
            t: 10,
            d: "/tmp".into(),
            x: 0,
            c: "target".into(),
            m: 0,
        };
        store.append(std::slice::from_ref(&entry)).unwrap();
        let rows = store.list().unwrap();

        // Corrupt everything after the first row; direct seek must still work.
        let mut f = OpenOptions::new().append(true).open(&store.path).unwrap();
        f.write_all(b"not json\n").unwrap();
        drop(f);

        let got = store.get(&rows[0].id).unwrap();
        assert_eq!(got, entry);
    }

    #[test]
    fn get_direct_and_fallback() {
        let store = test_store();
        let entries = vec![
            Entry {
                t: 10,
                d: "/tmp".into(),
                x: 0,
                c: "earlier".into(),
                m: 0,
            },
            Entry {
                t: 20,
                d: "/tmp".into(),
                x: 7,
                c: "target\ncontinued".into(),
                m: 0,
            },
            Entry {
                t: 30,
                d: "/tmp".into(),
                x: 0,
                c: "later".into(),
                m: 0,
            },
        ];
        store.append(&entries).unwrap();
        let rows = store.list().unwrap();

        let got = store.get(&rows[1].id).unwrap();
        assert_eq!(got, entries[1]);

        // Deleting an earlier row shifts offsets, so the direct seek misses and
        // the fallback (newest sequence match) must find the entry.
        store.delete(&rows[0].id, false).unwrap();
        let got = store.get(&rows[1].id).unwrap();
        assert_eq!(got, entries[1]);
    }

    #[test]
    fn exit_status_distinguishes_ids_and_fallback_get() {
        let store = test_store();
        let entries = vec![
            Entry {
                t: 5,
                d: "/tmp".into(),
                x: 0,
                c: "shift offsets".into(),
                m: 0,
            },
            Entry {
                t: 10,
                d: "/tmp".into(),
                x: 0,
                c: "same".into(),
                m: 0,
            },
            Entry {
                t: 10,
                d: "/tmp".into(),
                x: 7,
                c: "same".into(),
                m: 0,
            },
        ];
        store.append(&entries).unwrap();
        let rows = store.list().unwrap();
        assert_ne!(
            rows[1].id, rows[2].id,
            "different exit statuses share an id"
        );

        let ids = [rows[1].id.clone(), rows[2].id.clone()];
        store.delete(&rows[0].id, false).unwrap();
        for (i, id) in ids.iter().enumerate() {
            let got = store.get(id).unwrap();
            assert_eq!(got.x, entries[i + 1].x);
        }
    }

    #[test]
    fn duration_distinguishes_ids_and_fallback_get() {
        let store = test_store();
        let entries = vec![
            Entry {
                t: 5,
                d: "/tmp".into(),
                x: 0,
                c: "shift offsets".into(),
                m: 0,
            },
            Entry {
                t: 10,
                d: "/tmp".into(),
                x: 0,
                c: "same".into(),
                m: 100,
            },
            Entry {
                t: 10,
                d: "/tmp".into(),
                x: 0,
                c: "same".into(),
                m: 200,
            },
        ];
        store.append(&entries).unwrap();
        let rows = store.list().unwrap();
        assert_ne!(rows[1].id, rows[2].id, "different durations share an id");

        let ids = [rows[1].id.clone(), rows[2].id.clone()];
        store.delete(&rows[0].id, false).unwrap();
        for (i, id) in ids.iter().enumerate() {
            let got = store.get(id).unwrap();
            assert_eq!(got.m, entries[i + 1].m);
        }
    }

    #[test]
    fn duplicate_ids_and_delete() {
        let store = test_store();
        let duplicate = Entry {
            t: 10,
            d: "/tmp".into(),
            x: 0,
            c: "same".into(),
            m: 0,
        };
        let marker = Entry {
            t: 15,
            d: "/tmp".into(),
            x: 0,
            c: "marker".into(),
            m: 0,
        };
        let other = Entry {
            t: 20,
            d: "/tmp".into(),
            x: 0,
            c: "other".into(),
            m: 0,
        };
        store
            .append(&[
                duplicate.clone(),
                marker.clone(),
                duplicate.clone(),
                other.clone(),
            ])
            .unwrap();
        let rows = store.list().unwrap();
        assert_ne!(rows[0].id, rows[2].id, "duplicate entries share an id");

        store.delete(&rows[0].id, false).unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].entry, marker);
        assert_eq!(rows[1].entry, duplicate);
        assert_eq!(rows[2].entry, other);

        let same_command = vec![
            Entry {
                t: 30,
                d: "/one".into(),
                x: 1,
                c: "same".into(),
                m: 0,
            },
            Entry {
                t: 40,
                d: "/two".into(),
                x: 2,
                c: "same".into(),
                m: 0,
            },
        ];
        store.append(&same_command).unwrap();
        let rows = store.list().unwrap();
        store.delete(&rows[1].id, true).unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry, marker);
        assert_eq!(rows[1].entry, other);
    }

    #[test]
    fn delete_falls_back_to_seq_match_even_with_identical_content() {
        let store = test_store();
        let earlier = Entry {
            t: 5,
            c: "earlier".into(),
            d: String::new(),
            x: 0,
            m: 0,
        };
        let duplicate = Entry {
            t: 10,
            d: "/tmp".into(),
            x: 0,
            c: "same".into(),
            m: 0,
        };
        let marker = Entry {
            t: 15,
            c: "marker".into(),
            d: String::new(),
            x: 0,
            m: 0,
        };
        store
            .append(&[
                earlier,
                duplicate.clone(),
                marker.clone(),
                duplicate.clone(),
            ])
            .unwrap();
        let rows = store.list().unwrap();
        let stale_id = rows[1].id.clone();
        store.delete(&rows[0].id, false).unwrap();

        // stale_id's offset shifted; the sequence-number fallback must still
        // land on this exact row, not the other byte-identical `duplicate`.
        store.delete(&stale_id, false).unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry, marker);
        assert_eq!(rows[1].entry, duplicate);
    }

    #[test]
    fn delete_nonexistent_id() {
        let store = test_store();
        let entry = Entry {
            t: 10,
            d: "/tmp".into(),
            x: 0,
            c: "keep".into(),
            m: 0,
        };
        store.append(&[entry]).unwrap();
        let before = fs::read(&store.path).unwrap();

        store.delete("0-0", false).unwrap();
        let after = fs::read(&store.path).unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn append_tightens_loose_permissions_but_keeps_owner_bits() {
        let store = test_store();
        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        fs::write(&store.path, b"").unwrap();
        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o644)).unwrap();

        store
            .append(&[Entry {
                t: 1,
                d: String::new(),
                x: 0,
                c: "secret --token=x".into(),
                m: 0,
            }])
            .unwrap();
        let mode = fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "append left group/other able to read history");

        // Only group/other bits are looseness; the owner's own bits are the
        // user's business and must survive untouched.
        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o744)).unwrap();
        store
            .append(&[Entry {
                t: 2,
                d: String::new(),
                x: 0,
                c: "next".into(),
                m: 0,
            }])
            .unwrap();
        let mode = fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "append did not clear group/other, or clobbered owner bits"
        );
    }

    #[test]
    fn ids_are_offset_and_sequence_number() {
        let store = test_store();
        store
            .append(&[Entry {
                t: 1,
                d: String::new(),
                x: 0,
                c: "echo hi".into(),
                m: 0,
            }])
            .unwrap();
        let rows = store.list().unwrap();
        // First entry ever appended: offset 0 (first line), sequence 1 (the
        // counter starts at 0 and this is the first assignment).
        assert_eq!(rows[0].id, "0-1");
    }

    #[test]
    fn sequence_persists_across_separate_store_instances() {
        // Every `zhis` invocation constructs a fresh Store with no memory of
        // prior runs; the `.seq` sidecar is what keeps ids from colliding.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");

        Store::new(path.clone())
            .append(&[Entry {
                t: 1,
                d: String::new(),
                x: 0,
                c: "first".into(),
                m: 0,
            }])
            .unwrap();
        Store::new(path.clone())
            .append(&[Entry {
                t: 2,
                d: String::new(),
                x: 0,
                c: "second".into(),
                m: 0,
            }])
            .unwrap();

        let rows = Store::new(path).list().unwrap();
        assert_eq!(rows[0].id, "0-1");
        assert_ne!(
            rows[0].id, rows[1].id,
            "separate processes reused a sequence number"
        );
    }

    #[test]
    fn seq_recovers_from_last_line_when_sidecar_lost() {
        let store = test_store();
        store
            .append(&[
                Entry {
                    t: 1,
                    d: String::new(),
                    x: 0,
                    c: "one".into(),
                    m: 0,
                },
                Entry {
                    t: 2,
                    d: String::new(),
                    x: 0,
                    c: "two".into(),
                    m: 0,
                },
                Entry {
                    t: 3,
                    d: String::new(),
                    x: 0,
                    c: "three".into(),
                    m: 0,
                },
            ])
            .unwrap();
        let seq_path = PathBuf::from(format!("{}.seq", store.path.display()));
        assert_eq!(fs::read_to_string(&seq_path).unwrap(), "3");
        fs::remove_file(&seq_path).unwrap();

        // No sidecar, but the data file already holds ids 1..=3: the next
        // one must continue from there, not silently restart at 1.
        store
            .append(&[Entry {
                t: 4,
                d: String::new(),
                x: 0,
                c: "four".into(),
                m: 0,
            }])
            .unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 4);
        assert!(
            rows[3].id.ends_with("-4"),
            "recovered sequence collided with an existing id: {}",
            rows[3].id
        );
        assert_eq!(
            fs::read_to_string(&seq_path).unwrap(),
            "4",
            "sidecar not rebuilt after recovery"
        );
    }

    #[test]
    fn seq_recovers_from_last_line_when_sidecar_corrupt() {
        let store = test_store();
        store
            .append(&[
                Entry {
                    t: 1,
                    d: String::new(),
                    x: 0,
                    c: "one".into(),
                    m: 0,
                },
                Entry {
                    t: 2,
                    d: String::new(),
                    x: 0,
                    c: "two".into(),
                    m: 0,
                },
            ])
            .unwrap();
        let seq_path = PathBuf::from(format!("{}.seq", store.path.display()));
        // Present but unparsable, unlike the missing-sidecar case above.
        fs::write(&seq_path, "not a number").unwrap();

        store
            .append(&[Entry {
                t: 3,
                d: String::new(),
                x: 0,
                c: "three".into(),
                m: 0,
            }])
            .unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 3);
        assert!(
            rows[2].id.ends_with("-3"),
            "recovered sequence collided with an existing id: {}",
            rows[2].id
        );
    }

    #[test]
    fn negative_seq_triggers_recovery_instead_of_being_trusted() {
        let store = test_store();
        store
            .append(&[Entry {
                t: 1,
                d: String::new(),
                x: 0,
                c: "one".into(),
                m: 0,
            }])
            .unwrap();
        let seq_path = PathBuf::from(format!("{}.seq", store.path.display()));
        // A negative counter is as untrustworthy as a corrupt one.
        fs::write(&seq_path, "-5").unwrap();

        store
            .append(&[Entry {
                t: 2,
                d: String::new(),
                x: 0,
                c: "two".into(),
                m: 0,
            }])
            .unwrap();
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(
            rows[0].id, rows[1].id,
            "negative sidecar was trusted and reused an id"
        );
    }

    #[test]
    fn negative_recovered_seq_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        // No `.seq`; the data file's only (and thus last) line has i = -1.
        fs::write(
            &path,
            r#"{"i":-1,"t":1,"d":"","x":0,"c":"bad"}"#.to_string() + "\n",
        )
        .unwrap();

        let store = Store::new(path);
        let err = store.append(&[Entry {
            t: 2,
            d: String::new(),
            x: 0,
            c: "two".into(),
            m: 0,
        }]);
        assert!(
            err.is_err(),
            "a negative recovered sequence number must not be trusted"
        );
    }

    #[test]
    fn nth_line_start_from_end_cases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let at = |content: &[u8], n: usize| -> (i64, bool) {
            fs::write(&path, content).unwrap();
            let mut f = File::open(&path).unwrap();
            nth_line_start_from_end(&mut f, content.len() as i64, n).unwrap()
        };

        assert_eq!(at(b"", 1), (0, true));
        assert_eq!(at(b"abc", 0), (0, true));
        assert_eq!(at(b"ab\ncd\n", 1), (3, false));
        assert_eq!(
            at(b"ab\ncd\n", 2),
            (0, true),
            "n == line count is the whole file"
        );
        assert_eq!(at(b"ab\ncd\n", 9), (0, true));
        // No trailing newline: the last line still counts as a line.
        assert_eq!(at(b"ab\ncd", 1), (3, false));
        assert_eq!(at(b"a\nb\nc\nd\n", 3), (2, false));

        // Many short lines spanning several 32KB chunks, so the scan must
        // carry its count across chunk boundaries.
        let mut content = String::new();
        for i in 0..20_000 {
            content.push_str(&format!("line{}\n", i));
        }
        let (off, hit) = at(content.as_bytes(), 1);
        assert!(!hit);
        assert_eq!(&content[off as usize..], "line19999\n");
        let (off, hit) = at(content.as_bytes(), 15_000);
        assert!(!hit);
        assert_eq!(content[off as usize..].lines().next().unwrap(), "line5000");
        assert_eq!(content[off as usize..].lines().count(), 15_000);
    }

    #[test]
    fn limit_matches_the_unlimited_query_and_yields_usable_ids() {
        let store = test_store();
        let dirs = ["/a", "/b", "/a", "/c"];
        let entries: Vec<Entry> = (0..500)
            .map(|n| Entry {
                t: 1_700_000_000 + n,
                d: dirs[n as usize % dirs.len()].into(),
                x: 0,
                // Long enough that the file spans many 32KB scan chunks.
                c: format!("command number {} {}", n, "x".repeat(200)),
                m: 0,
            })
            .collect();
        store.append(&entries).unwrap();

        let all = store.query(&Query::default()).unwrap();
        assert_eq!(all.len(), 500);
        assert_eq!(
            all[0].entry.t,
            1_700_000_000 + 499,
            "query is not newest-first"
        );

        // A limit must return exactly the prefix the unlimited query returns —
        // same rows, same ids, including the offset baked into each id.
        for want in [1usize, 2, 7, 128, 499, 500, 501, 5000] {
            let got = store
                .query(&Query {
                    dir: None,
                    limit: Some(want),
                    uniq: false,
                })
                .unwrap();
            assert_eq!(
                got.len(),
                want.min(500),
                "limit {} returned {} rows",
                want,
                got.len()
            );
            for (g, a) in got.iter().zip(all.iter()) {
                assert_eq!(g.id, a.id, "limit {} produced a different id", want);
                assert_eq!(
                    g.entry, a.entry,
                    "limit {} produced a different entry",
                    want
                );
            }
        }

        // The ids from the backward path must still seek correctly, which is
        // what would break if read_rows tracked offsets relative to `from`.
        let tail = store
            .query(&Query {
                dir: None,
                limit: Some(3),
                uniq: false,
            })
            .unwrap();
        for row in &tail {
            assert_eq!(
                store.get(&row.id).unwrap(),
                row.entry,
                "id {} does not resolve",
                row.id
            );
        }
    }

    #[test]
    fn limit_with_sparse_dir_filter_widens_the_window() {
        let store = test_store();
        // One match every 50 rows, so `want` lines back cannot possibly hold
        // enough of them and the window has to grow.
        let entries: Vec<Entry> = (0..1000)
            .map(|n| Entry {
                t: 1_700_000_000 + n,
                d: if n % 50 == 0 {
                    "/rare".into()
                } else {
                    "/common".into()
                },
                x: 0,
                c: format!("cmd {}", n),
                m: 0,
            })
            .collect();
        store.append(&entries).unwrap();

        let all = store
            .query(&Query {
                dir: Some("/rare".into()),
                limit: None,
                uniq: false,
            })
            .unwrap();
        assert_eq!(all.len(), 20);
        for want in [1usize, 5, 20, 21] {
            let got = store
                .query(&Query {
                    dir: Some("/rare".into()),
                    limit: Some(want),
                    uniq: false,
                })
                .unwrap();
            assert_eq!(
                got.len(),
                want.min(20),
                "sparse limit {} gave {}",
                want,
                got.len()
            );
            for (g, a) in got.iter().zip(all.iter()) {
                assert_eq!(g.id, a.id);
                assert_eq!(g.entry.d, "/rare");
            }
        }
    }

    #[test]
    fn limit_on_missing_or_empty_store() {
        let store = test_store();
        let q = Query {
            dir: None,
            limit: Some(10),
            uniq: false,
        };
        assert!(
            store.query(&q).unwrap().is_empty(),
            "missing file should read as empty"
        );

        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        fs::write(&store.path, b"").unwrap();
        assert!(
            store.query(&q).unwrap().is_empty(),
            "empty file should read as empty"
        );

        store
            .append(&[Entry {
                t: 1,
                d: String::new(),
                x: 0,
                c: "only".into(),
                m: 0,
            }])
            .unwrap();
        assert_eq!(store.query(&q).unwrap().len(), 1);
        assert!(
            store
                .query(&Query {
                    dir: None,
                    limit: Some(0),
                    uniq: false,
                })
                .unwrap()
                .is_empty(),
            "limit 0 must mean zero rows here; main.rs maps it to None before this point"
        );
    }

    #[test]
    fn limit_reads_a_file_whose_last_line_lacks_a_newline() {
        let store = test_store();
        store
            .append(&[
                Entry {
                    t: 1,
                    d: "/x".into(),
                    x: 0,
                    c: "first".into(),
                    m: 0,
                },
                Entry {
                    t: 2,
                    d: "/x".into(),
                    x: 0,
                    c: "second".into(),
                    m: 0,
                },
            ])
            .unwrap();
        // Strip the trailing newline the way a crash mid-append could.
        let mut data = fs::read(&store.path).unwrap();
        assert_eq!(data.pop(), Some(b'\n'));
        fs::write(&store.path, &data).unwrap();

        let got = store
            .query(&Query {
                dir: None,
                limit: Some(1),
                uniq: false,
            })
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].entry.c, "second");
    }

    #[test]
    fn uniq_collapses_consecutive_repeats_to_newest() {
        let store = test_store();
        let entries = vec![
            Entry {
                t: 1,
                d: "/a".into(),
                x: 0,
                c: "one".into(),
                m: 0,
            },
            Entry {
                t: 2,
                d: "/a".into(),
                x: 0,
                c: "same".into(),
                m: 10,
            },
            Entry {
                t: 3,
                d: "/a".into(),
                x: 1,
                c: "same".into(),
                m: 20,
            },
            Entry {
                t: 4,
                d: "/a".into(),
                x: 0,
                c: "same".into(),
                m: 30,
            },
            Entry {
                t: 5,
                d: "/a".into(),
                x: 0,
                c: "two".into(),
                m: 0,
            },
        ];
        store.append(&entries).unwrap();

        let rows = store
            .query(&Query {
                dir: None,
                limit: None,
                uniq: true,
            })
            .unwrap();
        // Newest first; the run of "same" folds to its newest entry.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].entry.c, "two");
        assert_eq!(rows[1].entry.c, "same");
        assert_eq!(rows[1].entry.t, 4, "folded run keeps its newest entry");
        assert_eq!(rows[1].count, 3, "folded run did not count its repeats");
        assert_eq!(rows[0].count, 1);
        assert_eq!(rows[2].entry.c, "one");
        assert_eq!(rows[2].count, 1);
    }

    #[test]
    fn uniq_keeps_duplicates_separated_by_other_commands() {
        let store = test_store();
        let entries = vec![
            Entry {
                t: 1,
                d: "/a".into(),
                x: 0,
                c: "same".into(),
                m: 0,
            },
            Entry {
                t: 2,
                d: "/a".into(),
                x: 0,
                c: "other".into(),
                m: 0,
            },
            Entry {
                t: 3,
                d: "/a".into(),
                x: 0,
                c: "same".into(),
                m: 0,
            },
        ];
        store.append(&entries).unwrap();

        let rows = store
            .query(&Query {
                dir: None,
                limit: None,
                uniq: true,
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].entry.c, "same");
        assert_eq!(rows[1].entry.c, "other");
        assert_eq!(rows[2].entry.c, "same");
    }

    #[test]
    fn uniq_with_limit_reads_back_until_it_has_limit_rows() {
        let store = test_store();
        // The newest five are one run of repeats; dedupe collapses them to
        // one, so the limited read must widen past them to fetch older
        // distinct commands.
        let entries: Vec<Entry> = (0..10)
            .map(|n| Entry {
                t: 1_700_000_000 + n,
                d: "/a".into(),
                x: 0,
                c: if n >= 5 {
                    "repeat".into()
                } else {
                    format!("cmd{}", n)
                },
                m: 0,
            })
            .collect();
        store.append(&entries).unwrap();

        let rows = store
            .query(&Query {
                dir: None,
                limit: Some(5),
                uniq: true,
            })
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].entry.c, "repeat");
        assert_eq!(rows[0].entry.t, 1_700_000_000 + 9);
        assert_eq!(rows[1].entry.c, "cmd4");
        assert_eq!(rows[4].entry.c, "cmd1");
    }

    #[test]
    fn uniq_with_limit_returns_what_exists_when_shorter() {
        let store = test_store();
        let entries = vec![
            Entry {
                t: 1,
                d: "/a".into(),
                x: 0,
                c: "one".into(),
                m: 0,
            },
            Entry {
                t: 2,
                d: "/a".into(),
                x: 0,
                c: "one".into(),
                m: 0,
            },
            Entry {
                t: 3,
                d: "/a".into(),
                x: 0,
                c: "two".into(),
                m: 0,
            },
        ];
        store.append(&entries).unwrap();

        let rows = store
            .query(&Query {
                dir: None,
                limit: Some(10),
                uniq: true,
            })
            .unwrap();
        // Only two distinct commands exist; the limit cannot conjure more.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry.c, "two");
        assert_eq!(rows[1].entry.c, "one");
    }

    #[test]
    fn uniq_applies_within_the_dir_filter() {
        let store = test_store();
        let entries = vec![
            Entry {
                t: 1,
                d: "/a".into(),
                x: 0,
                c: "same".into(),
                m: 0,
            },
            Entry {
                t: 2,
                d: "/b".into(),
                x: 0,
                c: "same".into(),
                m: 0,
            },
            Entry {
                t: 3,
                d: "/a".into(),
                x: 0,
                c: "same".into(),
                m: 0,
            },
        ];
        store.append(&entries).unwrap();

        let rows = store
            .query(&Query {
                dir: Some("/a".into()),
                limit: None,
                uniq: true,
            })
            .unwrap();
        // Only /a rows remain; consecutive after the filter, so they collapse
        // to the newest. A dedupe before the filter would have kept t1.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.t, 3);
    }

    #[test]
    fn read_last_line_cases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");

        let read = |content: &[u8]| -> Option<Vec<u8>> {
            fs::write(&path, content).unwrap();
            let mut f = File::open(&path).unwrap();
            read_last_line(&mut f, content.len() as i64).unwrap()
        };

        assert_eq!(read(b""), None);
        assert_eq!(read(b"abc"), Some(b"abc".to_vec()));
        assert_eq!(read(b"abc\n"), Some(b"abc".to_vec()));
        assert_eq!(read(b"ab\ncd\n"), Some(b"cd".to_vec()));
        assert_eq!(read(b"ab\ncd"), Some(b"cd".to_vec()));
        assert_eq!(read(b"\n"), Some(b"".to_vec()));

        // A last line long enough to force the backward scan across more
        // than one 32KB chunk before it finds the preceding newline.
        let long_line = "x".repeat(100_000);
        let content = format!("first\n{}\n", long_line);
        assert_eq!(read(content.as_bytes()), Some(long_line.into_bytes()));

        // Same, but the trailing line isn't newline-terminated.
        let long_line = "y".repeat(100_000);
        let content = format!("first\n{}", long_line);
        assert_eq!(read(content.as_bytes()), Some(long_line.into_bytes()));
    }
}
