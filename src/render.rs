//! Everything `zhis list` prints to a row, and the row-layout contract that
//! `zhis init` hands to the shell so init.zsh's fzf flags can't drift from it.

use crate::store::Row;

pub const C_BLUE: &str = "\x1b[34m";
pub const C_DIM: &str = "\x1b[2m";
pub const C_RED: &str = "\x1b[31m";
pub const C_RESET: &str = "\x1b[0m";

// Unit separator, not tab — a tab can appear in a command's own text and
// would otherwise split the row into extra phantom fzf columns.
pub const FIELD_DELIM: char = '\x1f';
pub const ID_FIELD: usize = 1;
pub const DISPLAY_FIELDS: &str = "2..";

pub fn rel_time(t: i64, now: i64) -> String {
    let ago = now - t;
    if ago < 60 {
        format!("{:>2}s", ago)
    } else if ago < 3600 {
        format!("{:>2}m", ago / 60)
    } else if ago < 86400 {
        format!("{:>2}h", ago / 3600)
    } else if ago < 604800 {
        format!("{:>2}d", ago / 86400)
    } else {
        format!("{:>2}w", ago / 604800)
    }
}

/// Renders a duration in milliseconds; "" when unknown.
pub fn fmt_dur(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    if ms < 1000 {
        return format!("{}ms", ms);
    }
    if ms < 60_000 {
        // Truncate like the branches below; rounding could show "60.0s".
        return format!("{}.{}s", ms / 1000, ms % 1000 / 100);
    }
    if ms < 3_600_000 {
        return format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000);
    }
    format!("{}h{:02}m", ms / 3_600_000, ms % 3_600_000 / 60_000)
}

/// The command as the picker shows it: first line only for multiline
/// commands, marked with "⏎", and control bytes stripped.
fn display_command(c: &str) -> String {
    let disp = match c.find('\n') {
        Some(i) => format!("{} ⏎", &c[..i]),
        None => c.to_string(),
    };
    // A pasted \x1f (the field delimiter) or ANSI escape would split the row
    // into phantom columns or recolor it; strip control bytes from display.
    disp.chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect()
}

/// The one row-layout literal, so the two row kinds cannot drift apart.
/// FIELD_DELIM is invisible on screen, so each field's own trailing space
/// keeps a visible gap (fzf --with-nth reassembles using that same byte).
fn format_line(id: &str, dur: &str, ago: &str, col: &str, disp: &str) -> String {
    format!(
        "{id}{d}{dim}{dur:>7} {reset}{d}{blue}{ago:>7} {reset}{d}{col}{disp}{reset}",
        d = FIELD_DELIM,
        dim = C_DIM,
        reset = C_RESET,
        blue = C_BLUE,
    )
}

/// One `zhis list` line: id, duration, relative time, then the command.
pub fn format_row(row: &Row, now: i64) -> String {
    let e = &row.entry;
    format_line(
        &row.id,
        &fmt_dur(e.m),
        &rel_time(e.t, now),
        if e.x > 0 { C_RED } else { "" },
        &display_command(&e.c),
    )
}

/// A row for a command that is still running: the duration column counts up
/// from its start, and the command is dimmed rather than colored by an exit
/// status it does not have yet.
pub fn format_running_row(row: &Row, now: i64) -> String {
    let e = &row.entry;
    // Seconds resolution is all `t` carries; fmt_dur wants milliseconds.
    let elapsed = (now - e.t).max(0).saturating_mul(1000);
    format_line(
        &row.id,
        &fmt_dur(elapsed),
        "running",
        C_DIM,
        &display_command(&e.c),
    )
}

/// Emits the row layout as shell vars so init.zsh's fzf flags cannot drift
/// from cmd_list's writer. Not readonly: re-sourcing must not error.
pub fn zsh_layout_vars() -> String {
    format!(
        "typeset -g _zhis_delim=$'\\x{:02x}'\n\
         typeset -g _zhis_id_field={}\n\
         typeset -g _zhis_with_nth='{}'\n",
        FIELD_DELIM as u32, ID_FIELD, DISPLAY_FIELDS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_dur_cases() {
        let cases: &[(i64, &str)] = &[
            (0, ""),
            (-5, ""),
            (1, "1ms"),
            (999, "999ms"),
            (1000, "1.0s"),
            (1250, "1.2s"),
            (59_999, "59.9s"),
            (60_000, "1m00s"),
            (83_000, "1m23s"),
            (3_599_000, "59m59s"),
            (3_600_000, "1h00m"),
            (9_000_000, "2h30m"),
            (90_000_000, "25h00m"),
        ];
        for &(ms, want) in cases {
            assert_eq!(fmt_dur(ms), want, "fmt_dur({})", ms);
        }
    }

    #[test]
    fn format_row_strips_control_bytes_from_command() {
        use crate::store::Entry;
        let row = Row {
            entry: Entry {
                t: 0,
                d: "/d".into(),
                x: 0,
                c: "ls\x1f-a\x1b[31m".into(),
                m: 0,
            },
            id: "0-1".into(),
        };
        let out = format_row(&row, 0);
        // The row's own three delimiters are all that may appear; a command
        // carrying FIELD_DELIM would otherwise mint phantom fzf columns.
        assert_eq!(out.matches('\x1f').count(), 3, "row: {:?}", out);
        // The command's escape must not survive to recolor the picker.
        assert!(
            !out.contains("\x1b[31m"),
            "command escape leaked: {:?}",
            out
        );
        assert!(out.contains("ls-a"), "command text was mangled: {:?}", out);
    }

    #[test]
    fn format_running_row_keeps_the_row_contract() {
        use crate::store::Entry;
        let row = Row {
            entry: Entry {
                t: 100,
                d: "/d".into(),
                x: -1,
                c: "sleep 9\x1f\x1b[31m".into(),
                m: 0,
            },
            id: "@4242".into(),
        };
        let out = format_running_row(&row, 160);
        // Same three-delimiter contract format_row keeps: a command carrying
        // FIELD_DELIM must not mint phantom fzf columns.
        assert_eq!(out.matches('\x1f').count(), 3, "row: {:?}", out);
        assert!(
            !out.contains("\x1b[31m"),
            "command escape leaked: {:?}",
            out
        );
        assert!(out.contains("@4242"), "id lost: {:?}", out);
        assert!(out.contains("running"), "not marked as running: {:?}", out);
        // 60s elapsed, rendered from seconds since t rather than from `m`.
        assert!(out.contains("1m00s"), "elapsed not shown: {:?}", out);
    }

    #[test]
    fn rel_time_cases() {
        let now = 1_000_000;
        assert_eq!(rel_time(now - 5, now), " 5s");
        assert_eq!(rel_time(now - 59, now), "59s");
        assert_eq!(rel_time(now - 60, now), " 1m");
        assert_eq!(rel_time(now - 3599, now), "59m");
        assert_eq!(rel_time(now - 3600, now), " 1h");
        assert_eq!(rel_time(now - 86_399, now), "23h");
        assert_eq!(rel_time(now - 86_400, now), " 1d");
        assert_eq!(rel_time(now - 604_799, now), " 6d");
        assert_eq!(rel_time(now - 604_800, now), " 1w");
    }
}
