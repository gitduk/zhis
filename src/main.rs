// zhis is a shell history store with per-entry directory and exit status.
// Entries are appended as JSON lines. fzf provides the UI, wired up by the
// zsh integration that `zhis init` emits.
//
// This is a Rust port of github.com/overflowy/zhist.

mod cli;
mod import;
mod render;
mod store;

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::process::exit;
use std::time::{SystemTime, UNIX_EPOCH};

use cli::{die_err, die_usage, flags_or_die, int_flag};
use import::{import_history, import_jsonl};
use render::{format_row, zsh_layout_vars};
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

fn cmd_add(args: &[String]) {
    let flags = flags_or_die(args, &["dir", "exit", "ts", "ms"], &[]);
    let dir = flags.get("dir").cloned().unwrap_or_default();
    let exit_status = int_flag(&flags, "exit", -1);
    let ts = int_flag(&flags, "ts", 0);
    let ms = int_flag(&flags, "ms", 0);

    let mut raw = Vec::new();
    let _ = io::stdin().lock().read_to_end(&mut raw);
    let cmd = String::from_utf8_lossy(&raw)
        .trim_end_matches('\n')
        .to_string();
    if cmd.trim().is_empty() {
        return;
    }
    let t = if ts != 0 { ts } else { unix_now() };
    let m = ms.max(0);
    let entry = Entry {
        t,
        d: dir,
        x: exit_status,
        c: cmd,
        m,
    };
    if let Err(e) = Store::new(data_path()).append(&[entry]) {
        die_err(&e);
    }
}

fn cmd_list(args: &[String]) {
    let flags = flags_or_die(args, &["dir", "limit"], &[]);
    let dir = flags.get("dir").cloned().unwrap_or_default();
    let limit = int_flag(&flags, "limit", 0);
    let query = Query {
        dir: (!dir.is_empty()).then_some(dir),
        // A non-positive limit means unrestricted, so `-limit 0` can turn it
        // off without the caller special-casing the flag away.
        limit: (limit > 0).then_some(limit as usize),
    };
    let rows = match Store::new(data_path()).query(&query) {
        Ok(r) => r,
        Err(e) => die_err(&e),
    };
    let now = unix_now();

    let stdout = io::stdout();
    let mut w = BufWriter::new(stdout.lock());
    // Repeated runs each get a row; the picker stays faithful to what the
    // user did.
    for row in &rows {
        writeln!(w, "{}", format_row(row, now)).ok();
    }
    let _ = w.flush();
}

fn cmd_get(args: &[String]) {
    let flags = flags_or_die(args, &["id"], &[]);
    let id = flags.get("id").cloned().unwrap_or_default();
    match Store::new(data_path()).get(&id) {
        Ok(entry) => println!("{}", entry.c),
        Err(StoreError::EntryNotFound) => exit(1),
        Err(e) => die_err(&e),
    }
}

fn cmd_delete(args: &[String]) {
    let flags = flags_or_die(args, &["id"], &["all"]);
    let id = flags.get("id").cloned().unwrap_or_default();
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
        eprintln!("usage: zhis <init|add|list|get|delete|import|import-jsonl> [flags]");
        exit(2);
    }
    match args[0].as_str() {
        "init" => cmd_init(&args[1..]),
        "add" => cmd_add(&args[1..]),
        "list" => cmd_list(&args[1..]),
        "get" => cmd_get(&args[1..]),
        "delete" => cmd_delete(&args[1..]),
        "import" => cmd_import(&args[1..]),
        "import-jsonl" => cmd_import_jsonl(&args[1..]),
        other => {
            eprintln!("zhis: unknown command {}", other);
            exit(2);
        }
    }
}
