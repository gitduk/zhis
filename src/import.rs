//! Reads a zsh EXTENDED_HISTORY file for `zhis import`, or newline-delimited
//! JSON entries for `zhis import-jsonl`.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::store::{Entry, Store, StoreError, READ_BUF_CAP};

/// Parses a zsh EXTENDED_HISTORY header line: `: <t>:<n>;<cmd>`.
/// Returns the timestamp and the command text.
fn parse_zsh_hist_line(line: &str) -> Option<(i64, &str)> {
    let rest = line.strip_prefix(": ")?;
    let (t_str, rest) = rest.split_once(':')?;
    if t_str.is_empty() || !t_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (n_str, cmd) = rest.split_once(';')?;
    if n_str.is_empty() || !n_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let t = t_str.parse::<i64>().ok()?;
    Some((t, cmd))
}

/// Drops the one trailing backslash zsh writes as its continuation marker.
/// zsh keeps any backslash before it, so stripping every one loses a character.
fn strip_continuation(line: &str) -> &str {
    line.strip_suffix('\\').unwrap_or(line)
}

/// Reads a zsh EXTENDED_HISTORY file. Directory and exit status are unknown
/// for imported entries.
pub fn import_history(source: &Path, store: &Store) -> Result<usize, StoreError> {
    let f = File::open(source)?;
    let mut reader = BufReader::with_capacity(READ_BUF_CAP, f);
    let mut entries: Vec<Entry> = Vec::new();
    let mut cur: Option<Entry> = None;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(mut cur_entry) = cur.take() {
            // Continuation of a multiline entry.
            cur_entry.c.push('\n');
            cur_entry.c.push_str(strip_continuation(line));
            if line.ends_with('\\') {
                cur = Some(cur_entry);
            } else {
                entries.push(cur_entry);
            }
            continue;
        }
        if let Some((t, cmd)) = parse_zsh_hist_line(line) {
            if cmd.is_empty() {
                continue;
            }
            let entry = Entry {
                t,
                d: String::new(),
                x: -1,
                c: strip_continuation(cmd).to_string(),
                m: 0,
            };
            if cmd.ends_with('\\') {
                cur = Some(entry);
            } else {
                entries.push(entry);
            }
        }
    }
    if cur.is_some() {
        return Err(StoreError::Message(
            "unexpected end of file in multiline entry".to_string(),
        ));
    }
    store.append(&entries)?;
    Ok(entries.len())
}

/// Reads newline-delimited JSON entries — the shape zhis itself writes,
/// `{"t":..,"d":..,"x":..,"c":..,"m":..}` — and appends them in one batch.
pub fn import_jsonl(source: impl BufRead, store: &Store) -> Result<usize, StoreError> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in source.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: Entry = serde_json::from_str(&line)
            .map_err(|e| StoreError::Message(format!("invalid entry: {}", e)))?;
        entries.push(entry);
    }
    store.append(&entries)?;
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_zsh_hist_lines() {
        assert_eq!(
            parse_zsh_hist_line(": 123456:0;echo hi"),
            Some((123456, "echo hi"))
        );
        assert_eq!(parse_zsh_hist_line(": 123456:0;"), Some((123456, "")));
        assert_eq!(parse_zsh_hist_line("echo hi"), None);
        assert_eq!(parse_zsh_hist_line(": abc:0;echo hi"), None);
        assert_eq!(parse_zsh_hist_line(": 123456;echo hi"), None);
        assert_eq!(parse_zsh_hist_line(": 123456:0echo hi"), None);
    }

    #[test]
    fn import_keeps_literal_backslash_before_continuation() {
        // zsh parses `echo a\\` + newline + `next` as `echo a\` + newline +
        // `next`: only the marker goes, the literal backslash stays.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("zsh_history");
        std::fs::write(&src, ": 1700000000:0;echo a\\\\\nnext\n").unwrap();

        let store = Store::new(dir.path().join("history.jsonl"));
        assert_eq!(import_history(&src, &store).unwrap(), 1);
        let rows = store.list().unwrap();
        assert_eq!(rows[0].entry.c, "echo a\\\nnext");
    }

    #[test]
    fn import_jsonl_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("history.jsonl"));
        let ndjson = concat!(
            r#"{"t":10,"d":"/tmp","x":0,"c":"echo one","m":5}"#,
            "\n",
            "\n", // blank lines are skipped
            r#"{"t":20,"d":"/tmp","x":1,"c":"multi\nline"}"#,
            "\n",
        );
        let n = import_jsonl(ndjson.as_bytes(), &store).unwrap();
        assert_eq!(n, 2);

        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].entry,
            Entry {
                t: 10,
                d: "/tmp".into(),
                x: 0,
                c: "echo one".into(),
                m: 5
            }
        );
        assert_eq!(
            rows[1].entry,
            Entry {
                t: 20,
                d: "/tmp".into(),
                x: 1,
                c: "multi\nline".into(),
                m: 0
            }
        );
    }
}
