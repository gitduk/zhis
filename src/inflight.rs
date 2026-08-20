//! Commands that have started but not finished: one file per shell, named by
//! that shell's pid. The record hook writes history.jsonl from `precmd`, which
//! only runs once a command returns, so without this a long-running foreground
//! command is invisible to every other shell until it exits.
//!
//! No locking: a shell writes only the file its own pid names.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::{owner_only, Entry, Row};

/// Marks an id as naming an in-flight command. `Store` ids are
/// `<base36>-<base36>` with both halves non-negative, so `Store::parse_id`
/// rejects everything in this space and the two cannot collide.
const ID_SIGIL: char = '@';

/// One shell's current foreground command. No exit status or duration: it has
/// not finished. No pid either — the file name carries it.
#[derive(Serialize, Deserialize)]
struct Record {
    #[serde(rename = "t")]
    t: i64,
    #[serde(rename = "d")]
    d: String,
    #[serde(rename = "c")]
    c: String,
}

fn dir_for(data_path: &Path) -> PathBuf {
    let parent = data_path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(p) => p.join("inflight"),
        None => PathBuf::from("inflight"),
    }
}

fn file_for(data_path: &Path, pid: i32) -> PathBuf {
    dir_for(data_path).join(format!("{}.json", pid))
}

pub fn make_id(pid: i32) -> String {
    format!("{}{}", ID_SIGIL, pid)
}

/// The pid an in-flight id names, or None for anything else — including every
/// `Store` id, which is what keeps `get` and `delete` routing correctly.
pub fn parse_id(id: &str) -> Option<i32> {
    id.strip_prefix(ID_SIGIL)?.parse::<i32>().ok()
}

/// Does a process with this pid exist? EPERM means it exists but is not ours,
/// which still answers the question.
fn alive(pid: i32) -> bool {
    // Not just a range check: 0 means "every process in my group" and -1 means
    // "every process I may signal", so a junk file name must never reach kill.
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs the permission and existence checks
    // without delivering anything, and touches no memory we own.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Records the command `pid`'s shell just started.
///
/// Best-effort by design: every error is swallowed. This runs from `preexec`,
/// before every single command, and history.jsonl remains the system of
/// record — a diagnostic printed here would land in front of the user's own
/// output on every prompt, which is a worse failure than a picker that misses
/// a running command.
pub fn begin(data_path: &Path, pid: i32, dir: String, cmd: String, t: i64) {
    let _ = write_record(data_path, pid, Record { t, d: dir, c: cmd });
}

fn write_record(data_path: &Path, pid: i32, rec: Record) -> std::io::Result<()> {
    let dir = dir_for(data_path);
    fs::create_dir_all(&dir)?;
    // create_dir_all leaves the umask's bits; an existing tree keeps its own.
    if let Some(m) = owner_only(fs::metadata(&dir)?.permissions().mode()) {
        fs::set_permissions(&dir, fs::Permissions::from_mode(m))?;
    }

    let encoded = serde_json::to_vec(&rec).map_err(std::io::Error::other)?;
    // Written whole and renamed into place: a reader must never see half a
    // command, and rename is the only way to get that without a lock.
    let tmp = dir.join(format!(".{}.tmp", pid));
    let mut f = match open_new(&tmp) {
        // Only this pid writes this name, so a leftover is our own, from a
        // shell killed mid-write. create_new refuses to follow a symlink into
        // some other file; clearing and retrying keeps that guarantee.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(&tmp)?;
            open_new(&tmp)?
        }
        other => other?,
    };
    let result = f.write_all(&encoded);
    drop(f);
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, file_for(data_path, pid)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn open_new(path: &Path) -> std::io::Result<fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Drops `pid`'s in-flight record, called when its command finishes. A failure
/// here self-heals: the next command in that shell overwrites the same file.
pub fn end(data_path: &Path, pid: i32) {
    let _ = fs::remove_file(file_for(data_path, pid));
}

/// Every in-flight command, newest first, filtered by `dir` when given.
///
/// Sweeps as it reads: a record whose shell is gone (killed mid-command, so
/// `end` never ran) is unlinked instead of returned. Errors are swallowed for
/// the same reason as `begin` — this feeds a picker, not the store.
pub fn rows(data_path: &Path, dir: Option<&str>) -> Vec<Row> {
    let read_dir = match fs::read_dir(dir_for(data_path)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<(i32, Record)> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        let pid = match path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<i32>().ok())
        {
            Some(p) => p,
            None => continue,
        };
        if !alive(pid) {
            let _ = fs::remove_file(&path);
            continue;
        }
        let rec: Record = match fs::read(&path).ok().and_then(|b| {
            // A torn or truncated file is skipped, never reported: the shell
            // that owns it will rewrite it on its next command.
            serde_json::from_slice(&b).ok()
        }) {
            Some(r) => r,
            None => continue,
        };
        if rec.c.is_empty() {
            continue;
        }
        if dir.is_none_or(|d| rec.d == d) {
            out.push((pid, rec));
        }
    }

    out.sort_by_key(|(_, rec)| rec.t);
    out.into_iter()
        .rev()
        .map(|(pid, rec)| Row {
            id: make_id(pid),
            count: 1,
            entry: Entry {
                t: rec.t,
                d: rec.d,
                // The command has not finished, so both are genuinely unknown;
                // these are the same sentinels `zhis import` writes.
                x: -1,
                c: rec.c,
                m: 0,
            },
        })
        .collect()
}

/// The full command behind an in-flight id, for the picker's preview and for
/// accepting the row. None when the id is not in-flight or the shell is gone.
pub fn get(data_path: &Path, id: &str) -> Option<String> {
    let pid = parse_id(id)?;
    if !alive(pid) {
        return None;
    }
    let bytes = fs::read(file_for(data_path, pid)).ok()?;
    let rec: Record = serde_json::from_slice(&bytes).ok()?;
    (!rec.c.is_empty()).then_some(rec.c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Data path inside a temp dir; `inflight/` lands beside it.
    struct TestDir {
        _dir: tempfile::TempDir,
        data: PathBuf,
    }

    fn test_dir() -> TestDir {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("history.jsonl");
        TestDir { _dir: dir, data }
    }

    fn me() -> i32 {
        std::process::id() as i32
    }

    fn begin_default(t: &TestDir, pid: i32) {
        begin(&t.data, pid, "/work".into(), "sleep 100".into(), 42);
    }

    #[test]
    fn ids_do_not_collide_with_store_ids() {
        // Store ids are `<base36>-<base36>`; neither side may parse the other.
        assert_eq!(parse_id("@1234"), Some(1234));
        assert_eq!(parse_id("2f-19"), None);
        assert_eq!(parse_id("0-1"), None);
        assert_eq!(parse_id("@"), None);
        assert_eq!(parse_id("@abc"), None);
    }

    #[test]
    fn begin_then_get_round_trips() {
        let t = test_dir();
        begin_default(&t, me());
        assert_eq!(get(&t.data, &make_id(me())).as_deref(), Some("sleep 100"));

        let rows = rows(&t.data, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.c, "sleep 100");
        assert_eq!(rows[0].entry.t, 42);
        assert_eq!(rows[0].entry.x, -1, "an unfinished command has no status");
        assert_eq!(rows[0].entry.m, 0, "an unfinished command has no duration");
        assert_eq!(rows[0].id, make_id(me()));
    }

    #[test]
    fn end_removes_the_record() {
        let t = test_dir();
        begin_default(&t, me());
        end(&t.data, me());
        assert!(rows(&t.data, None).is_empty());
        assert_eq!(get(&t.data, &make_id(me())), None);
    }

    #[test]
    fn begin_overwrites_the_same_shells_previous_record() {
        let t = test_dir();
        begin(&t.data, me(), "/work".into(), "first".into(), 1);
        begin(&t.data, me(), "/other".into(), "second".into(), 2);
        let rows = rows(&t.data, None);
        assert_eq!(rows.len(), 1, "one shell has one foreground command");
        assert_eq!(rows[0].entry.c, "second");
        assert_eq!(rows[0].entry.d, "/other");
    }

    #[test]
    fn dead_shells_records_are_swept_not_returned() {
        let t = test_dir();
        // pid 1 is init and always alive; a record under a pid that cannot be
        // running is the kill -9 case `end` never got to clean up.
        let dead = i32::MAX;
        begin_default(&t, dead);
        assert!(file_for(&t.data, dead).exists());

        assert!(
            rows(&t.data, None).is_empty(),
            "a dead shell has no command"
        );
        assert!(
            !file_for(&t.data, dead).exists(),
            "the stale record must be swept, not left to accumulate"
        );
        assert_eq!(get(&t.data, &make_id(dead)), None);
    }

    #[test]
    fn dir_filter_selects_matching_records() {
        let t = test_dir();
        begin_default(&t, me());
        assert_eq!(rows(&t.data, Some("/work")).len(), 1);
        assert!(rows(&t.data, Some("/elsewhere")).is_empty());
    }

    #[test]
    fn newest_first() {
        let t = test_dir();
        // Two records need two pids that both survive the liveness sweep; this
        // process and its parent are the two guaranteed to be running.
        let parent = unsafe { libc::getppid() };
        assert_ne!(parent, me(), "need two distinct live pids");
        begin(&t.data, parent, "/work".into(), "older".into(), 10);
        begin(&t.data, me(), "/work".into(), "newer".into(), 20);

        let rows = rows(&t.data, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry.c, "newer");
        assert_eq!(rows[1].entry.c, "older");
    }

    #[test]
    fn record_files_are_not_group_or_world_readable() {
        let t = test_dir();
        begin(&t.data, me(), "/work".into(), "psql -W hunter2".into(), 42);
        let mode = fs::metadata(file_for(&t.data, me()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "commands hold secrets: mode {:o}", mode);
        let dmode = fs::metadata(dir_for(&t.data)).unwrap().permissions().mode();
        assert_eq!(
            dmode & 0o077,
            0,
            "directory leaks the set: mode {:o}",
            dmode
        );
    }

    #[test]
    fn junk_in_the_directory_is_ignored() {
        let t = test_dir();
        let dir = dir_for(&t.data);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notapid.json"), b"{}").unwrap();
        fs::write(dir.join("README"), b"hello").unwrap();
        fs::write(dir.join(format!("{}.json", me())), b"{not json").unwrap();
        assert!(rows(&t.data, None).is_empty());
        assert_eq!(get(&t.data, &make_id(me())), None);
    }

    #[test]
    fn a_leftover_temp_file_does_not_wedge_the_shell() {
        let t = test_dir();
        begin_default(&t, me());
        // What a SIGKILL between create and rename leaves behind.
        let tmp = dir_for(&t.data).join(format!(".{}.tmp", me()));
        fs::write(&tmp, b"garbage").unwrap();
        begin(&t.data, me(), "/work".into(), "after crash".into(), 43);
        assert_eq!(
            rows(&t.data, None)[0].entry.c,
            "after crash",
            "a stale temp file must not block later records"
        );
        assert!(!tmp.exists(), "temp file left behind: {:?}", tmp);
    }

    #[test]
    fn absent_directory_reads_as_empty() {
        let t = test_dir();
        assert!(rows(&t.data, None).is_empty());
        assert_eq!(get(&t.data, "@1"), None);
        assert_eq!(get(&t.data, "0-1"), None);
    }
}
