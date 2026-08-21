// zhis is a shell history store with per-entry directory and exit status.
// Entries are appended as JSON lines. fzf provides the UI, wired up by the
// zsh integration that `zhis init` emits.
//
// This is a Rust port of github.com/overflowy/zhist.

mod cli;
mod import;
mod inflight;
mod render;
mod store;

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{SystemTime, UNIX_EPOCH};

use cli::{die_err, die_usage, flags_or_die, int_flag};
use import::{import_history, import_jsonl};
use render::{format_row, format_running_row, preview_lines, zsh_layout_vars};
use store::{Entry, Query, Store, StoreError};

// The zsh integration is emitted by `zhis init`; see zsh/init.zsh.
const ZSH_INIT: &str = include_str!("../zsh/init.zsh");
const ZSH_ARROW_BINDS: &str = include_str!("../zsh/arrows.zsh");

fn data_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("ZHIS_FILE") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => {
            std::path::PathBuf::from(home).join(".local/share/zhis/history.jsonl")
        }
        _ => die_usage("zhis: $HOME is not set; set $ZHIS_FILE instead"),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The piped command text; None for a blank or whitespace-only read.
fn read_piped_cmd() -> Option<String> {
    let mut raw = Vec::new();
    let _ = io::stdin().lock().read_to_end(&mut raw);
    let cmd = String::from_utf8_lossy(&raw)
        .trim_end_matches('\n')
        .to_string();
    (!cmd.trim().is_empty()).then_some(cmd)
}

fn ts_or_now(ts: i64) -> i64 {
    if ts != 0 {
        ts
    } else {
        unix_now()
    }
}

fn cmd_add(args: &[String]) {
    let flags = flags_or_die(args, &["dir", "exit", "ts", "ms", "pid"], &[]);
    let dir = flags.get("dir").cloned().unwrap_or_default();
    let exit_status = int_flag(&flags, "exit", -1);
    let ts = int_flag(&flags, "ts", 0);
    let ms = int_flag(&flags, "ms", 0);

    let Some(cmd) = read_piped_cmd() else {
        return;
    };
    let t = ts_or_now(ts);
    let m = ms.max(0);
    let entry = Entry {
        t,
        d: dir,
        x: exit_status,
        c: cmd,
        m,
    };
    let path = data_path();
    // Clear first: the command has finished either way, and a store error must
    // not strand a record that would then show as still running.
    if let Some(pid) = pid_flag(&flags) {
        inflight::end(&path, pid);
    }
    if let Err(e) = Store::new(path).append(&[entry]) {
        die_err(&e);
    }
}

/// Records a command as started. Paired with the `-pid` on `add`, which clears
/// it again; see the preexec hook in zsh/init.zsh.
fn cmd_begin(args: &[String]) {
    let flags = flags_or_die(args, &["dir", "ts", "pid"], &[]);
    let dir = flags.get("dir").cloned().unwrap_or_default();
    let ts = int_flag(&flags, "ts", 0);
    let pid = match pid_flag(&flags) {
        Some(p) => p,
        None => die_usage("usage: zhis begin -pid N [-dir D] [-ts N]"),
    };

    let Some(cmd) = read_piped_cmd() else {
        return;
    };
    inflight::begin(&data_path(), pid, dir, cmd, ts_or_now(ts));
}

/// The shell's own pid (`$$`), which is not this process's — `zhis` runs as a
/// child of the hook, so the caller must pass it. An unusable value warns and
/// reads as absent: history.jsonl is the system of record, and `add` must not
/// lose an entry over the in-flight bookkeeping flag.
fn pid_flag(flags: &cli::Flags) -> Option<i32> {
    if !flags.has("pid") {
        return None;
    }
    match i32::try_from(int_flag(flags, "pid", 0)) {
        Ok(p) if p > 0 => Some(p),
        _ => {
            eprintln!("zhis: ignoring -pid: not a usable process id");
            None
        }
    }
}

fn cmd_list(args: &[String]) {
    let flags = flags_or_die(args, &["dir", "limit"], &["uniq"]);
    let dir = flags.get("dir").cloned().unwrap_or_default();
    let limit = int_flag(&flags, "limit", 0);
    let query = Query {
        dir: (!dir.is_empty()).then_some(dir),
        // A non-positive limit means unrestricted, so `-limit 0` can turn it
        // off without the caller special-casing the flag away.
        limit: (limit > 0).then_some(limit as usize),
        uniq: flags.has("uniq"),
    };
    let path = data_path();
    // Deliberately outside `-limit`: what is running now is what the user most
    // wants to see, and there are only ever as many as there are live shells.
    let running = inflight::rows(&path, query.dir.as_deref());
    let rows = match Store::new(path).query(&query) {
        Ok(r) => r,
        Err(e) => die_err(&e),
    };
    let now = unix_now();

    let stdout = io::stdout();
    let mut w = BufWriter::new(stdout.lock());
    for row in &running {
        writeln!(w, "{}", format_running_row(row, now)).ok();
    }
    // Repeated runs each get a row; the picker stays faithful to what the
    // user did.
    for row in &rows {
        writeln!(w, "{}", format_row(row, now)).ok();
    }
    let _ = w.flush();
}

/// The command an id names, whether it is a finished entry or a live one.
/// None for an id the store no longer holds; callers exit 1 on it, which is
/// how the picker learns a row went away under it.
fn command_for_id(path: PathBuf, id: &str) -> Option<String> {
    if inflight::parse_id(id).is_some() {
        return inflight::get(&path, id);
    }
    match Store::new(path).get(id) {
        Ok(entry) => Some(entry.c),
        Err(StoreError::EntryNotFound) => None,
        Err(e) => die_err(&e),
    }
}

fn cmd_get(args: &[String]) {
    let flags = flags_or_die(args, &["id"], &[]);
    let id = flags.get("id").cloned().unwrap_or_default();
    match command_for_id(data_path(), &id) {
        Some(cmd) => println!("{}", cmd),
        None => exit(1),
    }
}

/// How tall a preview the picker must open for a row, asked on every focus;
/// see the fit transform in zsh/init.zsh. Exit 1 means "show no preview".
/// -count is the folded row's " ×N" field; the picker passes {5}, which
/// fzf hands over with ANSI stripped and the space trimmed.
fn cmd_fit(args: &[String]) {
    let flags = flags_or_die(args, &["id", "cols", "count"], &[]);
    let id = flags.get("id").cloned().unwrap_or_default();
    // Not int_flag: an empty $FZF_COLUMNS is a fallback, not a usage error.
    let cols = flags
        .get("cols")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let count = flags
        .get("count")
        .and_then(|v| {
            let v = v.trim();
            v.strip_prefix('×').unwrap_or(v).parse::<usize>().ok()
        })
        .unwrap_or(0);
    match command_for_id(data_path(), &id).and_then(|cmd| preview_lines(&cmd, cols, count)) {
        Some(n) => println!("{}", n),
        None => exit(1),
    }
}

fn cmd_delete(args: &[String]) {
    let flags = flags_or_die(args, &["id"], &["all"]);
    let id = flags.get("id").cloned().unwrap_or_default();
    // A running command is not the picker's to delete: the record clears itself
    // when the command ends, and removing it would not stop the process.
    if inflight::parse_id(&id).is_some() {
        return;
    }
    let all = flags.has("all");
    if let Err(e) = Store::new(data_path()).delete(&id, all) {
        die_err(&e);
    }
}

fn cmd_import(args: &[String]) {
    let flags = flags_or_die(args, &[], &[]);
    if flags.positionals.len() != 1 {
        eprintln!("usage: zhis import <zsh_history_file>");
        exit(2);
    }
    let n = match import_history(Path::new(&flags.positionals[0]), &Store::new(data_path())) {
        Ok(n) => n,
        Err(e) => die_err(&e),
    };
    println!("imported {} entries", n);
}

fn cmd_import_jsonl(args: &[String]) {
    let flags = flags_or_die(args, &[], &[]);
    let store = Store::new(data_path());
    let result = match flags.positionals.first() {
        Some(path) => File::open(path)
            .map_err(StoreError::from)
            .and_then(|f| import_jsonl(io::BufReader::new(f), &store)),
        None => import_jsonl(io::BufReader::new(io::stdin().lock()), &store),
    };
    match result {
        Ok(n) => println!("imported {} entries", n),
        Err(e) => die_err(&e),
    }
}

fn cmd_init(args: &[String]) {
    let flags = flags_or_die(args, &[], &["no-arrow-binds"]);
    print!("{}", zsh_layout_vars());
    print!("{}", ZSH_INIT);
    if !flags.has("no-arrow-binds") {
        print!("{}", ZSH_ARROW_BINDS);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: zhis <init|add|begin|list|get|fit|delete|import|import-jsonl> [flags]");
        exit(2);
    }
    match args[0].as_str() {
        "init" => cmd_init(&args[1..]),
        "add" => cmd_add(&args[1..]),
        "begin" => cmd_begin(&args[1..]),
        "list" => cmd_list(&args[1..]),
        "get" => cmd_get(&args[1..]),
        "fit" => cmd_fit(&args[1..]),
        "delete" => cmd_delete(&args[1..]),
        "import" => cmd_import(&args[1..]),
        "import-jsonl" => cmd_import_jsonl(&args[1..]),
        other => {
            eprintln!("zhis: unknown command {}", other);
            exit(2);
        }
    }
}
