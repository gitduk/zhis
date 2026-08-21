//! End-to-end tests against the real binary: exit-code conventions, stdin
//! handling, and the row-layout contract `zhis init` hands to the shell.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn hist(dir: &Path) -> PathBuf {
    dir.join("history.jsonl")
}

/// The binary with an isolated store. HOME is redirected too, so a bug in
/// `data_path`'s precedence cannot reach the developer's real history.
fn zhis(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_zhis"));
    c.env("ZHIS_FILE", hist(dir)).env("HOME", dir);
    c
}

fn run(dir: &Path, args: &[&str]) -> Output {
    zhis(dir).args(args).output().unwrap()
}

fn add(dir: &Path, cmd: &str, args: &[&str]) -> Output {
    let mut child = zhis(dir)
        .arg("add")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(cmd.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn no_args_and_unknown_command_are_usage_errors() {
    let dir = tempfile::tempdir().unwrap();

    let out = run(dir.path(), &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));

    let out = run(dir.path(), &["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown command"));
}

#[test]
fn undefined_flag_is_usage_error_not_store_error() {
    let dir = tempfile::tempdir().unwrap();
    // Exit 2 vs 1 is the whole distinction: a caller's mistake, not an I/O
    // failure. `list` would otherwise succeed on an absent store.
    let out = run(dir.path(), &["list", "-bogus", "x"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!hist(dir.path()).exists());
}

#[test]
fn add_reads_stdin_then_list_and_get_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    assert!(add(
        dir.path(),
        "echo one\n",
        &["-dir", "/tmp/a", "-exit", "0", "-ms", "1250"]
    )
    .status
    .success());
    assert!(add(
        dir.path(),
        "printf a\nprintf b",
        &["-dir", "/tmp/b", "-exit", "1"]
    )
    .status
    .success());

    let rows = stdout(&run(dir.path(), &["list"]));
    let lines: Vec<&str> = rows.lines().collect();
    assert_eq!(lines.len(), 2);
    // Newest first, and a multiline command collapses to its first line.
    assert!(lines[0].contains("printf a"), "{:?}", lines[0]);
    assert!(
        !lines[0].contains("printf b"),
        "multiline leaked a second row"
    );
    assert!(
        lines[0].contains('⏎'),
        "no multiline marker: {:?}",
        lines[0]
    );
    assert!(
        lines[1].contains("echo one") && lines[1].contains("1.2s"),
        "{:?}",
        lines[1]
    );

    // -dir filters, and the newline `add` strips must not survive into storage.
    let scoped = stdout(&run(dir.path(), &["list", "-dir", "/tmp/a"]));
    assert_eq!(scoped.lines().count(), 1);
    assert!(scoped.contains("echo one"));

    let id = first_field(dir.path(), lines[0]);
    let got = stdout(&run(dir.path(), &["get", "-id", &id]));
    assert_eq!(
        got, "printf a\nprintf b\n",
        "get did not return the full command"
    );
}

#[test]
fn add_ignores_blank_command_without_creating_a_store() {
    let dir = tempfile::tempdir().unwrap();
    assert!(add(dir.path(), "   \n", &["-dir", "/tmp", "-exit", "0"])
        .status
        .success());
    assert!(
        !hist(dir.path()).exists(),
        "a blank command created a history file"
    );
    assert_eq!(stdout(&run(dir.path(), &["list"])), "");
}

#[test]
fn get_with_unknown_id_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    add(dir.path(), "present", &["-dir", "/tmp", "-exit", "0"]);
    // Well-formed but unassigned, and malformed: both are "not found", never a
    // panic and never exit 0 with empty output.
    for id in ["zzz-zzz", "not-an-id-at-all"] {
        let out = run(dir.path(), &["get", "-id", id]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "get -id {} exited {:?}",
            id,
            out.status.code()
        );
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn delete_all_removes_every_row_with_that_command() {
    let dir = tempfile::tempdir().unwrap();
    for (i, cmd) in ["dup", "keep", "dup"].iter().enumerate() {
        add(
            dir.path(),
            cmd,
            &[
                "-dir",
                "/tmp",
                "-exit",
                "0",
                "-ts",
                &(100 + i as i64).to_string(),
            ],
        );
    }
    let rows = stdout(&run(dir.path(), &["list"]));
    let dup_line = rows.lines().find(|l| l.contains("dup")).unwrap();
    let id = first_field(dir.path(), dup_line);

    assert!(run(dir.path(), &["delete", "-id", &id, "-all"])
        .status
        .success());
    let rows = stdout(&run(dir.path(), &["list"]));
    assert_eq!(rows.lines().count(), 1);
    assert!(rows.contains("keep"));

    // Deleting an already-gone id is a no-op, not an error.
    assert!(run(dir.path(), &["delete", "-id", &id]).status.success());
}

#[test]
fn list_limit_returns_the_newest_rows_and_zero_means_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..20 {
        add(
            dir.path(),
            &format!("cmd{:02}", n),
            &[
                "-dir",
                "/tmp",
                "-exit",
                "0",
                "-ts",
                &(1_700_000_000 + n).to_string(),
            ],
        );
    }

    let full = stdout(&run(dir.path(), &["list"]));
    assert_eq!(full.lines().count(), 20);

    let limited = stdout(&run(dir.path(), &["list", "-limit", "3"]));
    assert_eq!(limited.lines().count(), 3);
    // Byte-identical to the head of the unlimited output, ids included.
    assert_eq!(
        limited.lines().collect::<Vec<_>>(),
        full.lines().take(3).collect::<Vec<_>>()
    );
    assert!(limited.contains("cmd19") && !limited.contains("cmd16"));

    // Non-positive means unrestricted, so callers can pass it unconditionally.
    for zero in ["0", "-1"] {
        let out = stdout(&run(dir.path(), &["list", "-limit", zero]));
        assert_eq!(
            out.lines().count(),
            20,
            "-limit {} restricted the output",
            zero
        );
    }
    // A limit past the end is not an error.
    assert_eq!(
        stdout(&run(dir.path(), &["list", "-limit", "999"]))
            .lines()
            .count(),
        20
    );

    let out = run(dir.path(), &["list", "-limit", "abc"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a non-numeric limit must be a usage error"
    );
}

#[test]
fn list_uniq_collapses_consecutive_repeats_to_newest() {
    let dir = tempfile::tempdir().unwrap();
    // Three "dup" in a row, then "keep", then two more "dup": each run
    // collapses to one row, and the later run keeps the newer timestamp.
    for (cmd, n) in [
        ("dup", 1),
        ("dup", 2),
        ("dup", 3),
        ("keep", 4),
        ("dup", 5),
        ("dup", 6),
    ] {
        add(
            dir.path(),
            cmd,
            &[
                "-dir",
                "/tmp",
                "-exit",
                "0",
                "-ts",
                &(1_700_000_000 + n).to_string(),
            ],
        );
    }

    let full = stdout(&run(dir.path(), &["list"]));
    assert_eq!(full.lines().count(), 6);

    let uniq = stdout(&run(dir.path(), &["list", "-uniq"]));
    let lines: Vec<&str> = uniq.lines().collect();
    assert_eq!(lines.len(), 3, "got: {:?}", lines);
    // Newest first: the trailing run of "dup", then "keep", then the first
    // run's newest "dup".
    assert!(lines[0].contains("dup"));
    assert!(lines[1].contains("keep"));
    assert!(lines[2].contains("dup"));
}

#[test]
fn list_uniq_with_limit_returns_limit_distinct_commands() {
    let dir = tempfile::tempdir().unwrap();
    // cmd0..cmd3 then a run of "dup": limit 3 + uniq must widen past the run
    // to return dup plus the two newest distinct commands.
    for (cmd, n) in [
        ("cmd0", 0),
        ("cmd1", 1),
        ("cmd2", 2),
        ("cmd3", 3),
        ("dup", 4),
        ("dup", 5),
        ("dup", 6),
    ] {
        add(
            dir.path(),
            cmd,
            &[
                "-dir",
                "/tmp",
                "-exit",
                "0",
                "-ts",
                &(1_700_000_000 + n).to_string(),
            ],
        );
    }

    let out = stdout(&run(dir.path(), &["list", "-limit", "3", "-uniq"]));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "got: {:?}", lines);
    assert!(lines[0].contains("dup"));
    assert!(lines[1].contains("cmd3"));
    assert!(lines[2].contains("cmd2"));
}

#[test]
fn init_layout_vars_match_what_list_actually_emits() {
    let dir = tempfile::tempdir().unwrap();
    add(dir.path(), "cargo build", &["-dir", "/tmp", "-exit", "0"]);

    let script = stdout(&run(dir.path(), &["init"]));
    let delim = parse_delim(&script);
    let id_field: usize = var(&script, "_zhis_id_field").parse().unwrap();
    assert_eq!(var(&script, "_zhis_with_nth"), "'2..'");

    let row = stdout(&run(dir.path(), &["list"]));
    let row = row.lines().next().unwrap();
    let fields: Vec<&str> = row.split(delim).collect();
    assert!(
        fields.len() >= 2,
        "row has no {:?} separator: {:?}",
        delim,
        row
    );

    // The id init tells the shell to cut must be the id `get` accepts — this
    // is the contract render.rs exists to keep from drifting.
    let id = fields[id_field - 1];
    let got = run(dir.path(), &["get", "-id", id]);
    assert!(
        got.status.success(),
        "id from field {} was rejected by get",
        id_field
    );
    assert_eq!(stdout(&got), "cargo build\n");
    assert!(
        !id.contains(' ') && id.contains('-'),
        "unexpected id shape: {:?}",
        id
    );
}

#[test]
fn init_arrow_binds_are_opt_out() {
    let dir = tempfile::tempdir().unwrap();

    // Matches the key literal, not the whole bindkey line: which keymaps each
    // bind names is a separate decision from whether the bind is emitted.
    let with = stdout(&run(dir.path(), &["init"]));
    assert!(
        with.contains("'^[[A'"),
        "default init lost the up-arrow bind"
    );
    assert!(with.contains("'^R'"));

    let without = stdout(&run(dir.path(), &["init", "-no-arrow-binds"]));
    assert!(
        !without.contains("'^[[A'"),
        "-no-arrow-binds still bound up-arrow"
    );
    assert!(
        without.contains("'^R'"),
        "-no-arrow-binds dropped ctrl-r too"
    );
}

#[test]
fn init_defaults_the_dedupe_toggle_on() {
    let dir = tempfile::tempdir().unwrap();
    let out = stdout(&run(dir.path(), &["init"]));
    assert!(
        out.contains("print -r -- on > \"$ufile\""),
        "init does not default the dedupe toggle to on"
    );
}

#[test]
fn ctrl_r_is_bound_in_every_keymap_the_user_types_in() {
    let dir = tempfile::tempdir().unwrap();
    let out = stdout(&run(dir.path(), &["init"]));
    for km in ["emacs", "viins", "vicmd"] {
        assert!(
            out.contains(&format!("bindkey -M {} '^R'", km)),
            "ctrl-r unbound in {}: a bare bindkey only hits the current keymap",
            km
        );
    }
}

#[test]
fn init_pins_the_preview_geometry_its_height_math_assumes() {
    let dir = tempfile::tempdir().unwrap();
    let out = stdout(&run(dir.path(), &["init"]));
    // The awk's `w - 4` is fzf's rounded border plus its padding, measured;
    // a wrap sign would narrow continuation lines out of that one width.
    assert!(
        out.contains("--preview-border=rounded"),
        "preview border unpinned: `w - 4` no longer describes the text column"
    );
    assert!(
        out.contains("--preview-wrap-sign=\"\""),
        "wrap sign back on: continuation lines are 2 columns narrower"
    );
    assert!(
        out.contains("--tabstop=1"),
        "tabstop unpinned: display_width counts a tab as one column"
    );
}

#[test]
fn fit_bounds_the_folded_row_by_its_real_count() {
    let dir = tempfile::tempdir().unwrap();
    // 99 columns fits a plain row (115 - 14) but not one folded as " ×2"
    // (115 - 14 - 3), so the count the picker passes must decide the bound.
    let long = "x".repeat(99);
    for _ in 0..2 {
        add(dir.path(), &long, &["-dir", "/tmp", "-exit", "0"]);
    }
    let uniq = stdout(&run(dir.path(), &["list", "-uniq"]));
    let id = first_field(dir.path(), uniq.lines().next().unwrap());
    let fit = |count: &str| {
        run(
            dir.path(),
            &["fit", "-id", &id, "-cols", "115", "-count", count],
        )
    };

    // Without the count the row is judged to show the whole command.
    assert!(!fit("").status.success());
    // The folded suffix " ×2" narrows the row to 98: a preview is needed.
    assert_eq!(stdout(&fit("×2")).trim(), "1");
}

#[test]
fn fit_sizes_the_preview_from_the_row_the_picker_will_draw() {
    let dir = tempfile::tempdir().unwrap();
    let long = "x".repeat(113);
    add(dir.path(), &long, &["-dir", "/tmp", "-exit", "0"]);
    let row = stdout(&run(dir.path(), &["list"]));
    let id = first_field(dir.path(), row.lines().next().unwrap());
    let fit = |cols: &str| run(dir.path(), &["fit", "-id", &id, "-cols", cols]);

    // 115 columns leaves the preview 111, so 113 needs a second line.
    assert_eq!(stdout(&fit("115")).trim(), "2");
    // Wide enough that the row itself shows the whole command: no preview,
    // which the picker reads off the exit status.
    assert!(!fit("200").status.success());
    // An empty $FZF_COLUMNS is a fallback (80 columns), not a usage error.
    assert_eq!(stdout(&fit("")).trim(), "2");
    // A row deleted under the picker must hide the preview, not error out.
    let gone = run(dir.path(), &["fit", "-id", "0-999", "-cols", "115"]);
    assert!(!gone.status.success());
    assert_eq!(
        gone.status.code(),
        Some(1),
        "a gone row must exit 1 (the no-preview signal), not succeed"
    );
}

#[test]
fn import_jsonl_reads_stdin_and_reports_count() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = zhis(dir.path())
        .arg("import-jsonl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let ndjson = concat!(
        r#"{"t":10,"d":"/tmp","x":0,"c":"one","m":5}"#,
        "\n\n",
        r#"{"t":20,"d":"/tmp","x":1,"c":"two"}"#,
        "\n",
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(ndjson.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(stdout(&out), "imported 2 entries\n");
    assert_eq!(stdout(&run(dir.path(), &["list"])).lines().count(), 2);
}

/// Splits a `zhis list` row on the delimiter `zhis init` declares and returns
/// the id field, so tests never hardcode the layout.
fn first_field(dir: &Path, row: &str) -> String {
    let script = stdout(&run(dir, &["init"]));
    let delim = parse_delim(&script);
    let idx: usize = var(&script, "_zhis_id_field").parse().unwrap();
    row.split(delim).nth(idx - 1).unwrap().to_string()
}

fn var(script: &str, name: &str) -> String {
    let prefix = format!("typeset -g {}=", name);
    script
        .lines()
        .find_map(|l| l.trim().strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("{} not set by `zhis init`", name))
        .to_string()
}

/// `_zhis_delim=$'\x1f'` -> the byte itself.
fn parse_delim(script: &str) -> char {
    let raw = var(script, "_zhis_delim");
    let hex = raw
        .trim_start_matches("$'")
        .trim_end_matches('\'')
        .strip_prefix("\\x")
        .unwrap_or_else(|| panic!("unexpected delim literal: {}", raw));
    char::from(u8::from_str_radix(hex, 16).unwrap())
}
