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

// Seconds per rung of rel_time's ladder. A year is 52 weeks, not the
// calendar's 365 days, so the boundary lands on the rung it replaces.
const S_MIN: i64 = 60;
const S_HOUR: i64 = 60 * S_MIN;
const S_DAY: i64 = 24 * S_HOUR;
const S_WEEK: i64 = 7 * S_DAY;
const S_YEAR: i64 = 52 * S_WEEK;

pub fn rel_time(t: i64, now: i64) -> String {
    let ago = now - t;
    if ago < S_MIN {
        format!("{:>2}s", ago)
    } else if ago < S_HOUR {
        format!("{:>2}m", ago / S_MIN)
    } else if ago < S_DAY {
        format!("{:>2}h", ago / S_HOUR)
    } else if ago < S_WEEK {
        format!("{:>2}d", ago / S_DAY)
    } else if ago < S_YEAR {
        format!("{:>2}w", ago / S_WEEK)
    } else {
        format!("{:>2}y", ago / S_YEAR)
    }
}

// Milliseconds per rung of fmt_dur's ladder. Month and year are stipulated,
// not the calendar's: only then can the year rung never print "1y12m".
const MS_SEC: i64 = 1_000;
const MS_MIN: i64 = 60 * MS_SEC;
const MS_HOUR: i64 = 60 * MS_MIN;
const MS_DAY: i64 = 24 * MS_HOUR;
const MS_MONTH: i64 = 30 * MS_DAY;
const MS_YEAR: i64 = 12 * MS_MONTH;

/// Renders a duration in milliseconds as its two largest units; "" when
/// unknown.
pub fn fmt_dur(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    if ms < MS_SEC {
        return format!("{}ms", ms);
    }
    if ms < MS_MIN {
        // Truncate like the rungs below; rounding could show "60.0s".
        return format!("{}.{}s", ms / MS_SEC, ms % MS_SEC / 100);
    }
    if ms < MS_HOUR {
        return format!("{}m{:02}s", ms / MS_MIN, ms % MS_MIN / MS_SEC);
    }
    if ms < MS_DAY {
        return format!("{}h{:02}m", ms / MS_HOUR, ms % MS_HOUR / MS_MIN);
    }
    if ms < MS_MONTH {
        return format!("{}d{:02}h", ms / MS_DAY, ms % MS_DAY / MS_HOUR);
    }
    if ms < MS_YEAR {
        return format!("{}m{:02}d", ms / MS_MONTH, ms % MS_MONTH / MS_DAY);
    }
    format!("{}y{:02}m", ms / MS_YEAR, ms % MS_YEAR / MS_MONTH)
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
fn format_line(id: &str, dur: &str, ago: &str, count: &str, col: &str, disp: &str) -> String {
    format!(
        "{id}{d}{dim}{dur:>6} {reset}{d}{blue}{ago:>3} {reset}{d}{col}{disp}{count}{reset}",
        d = FIELD_DELIM,
        dim = C_DIM,
        reset = C_RESET,
        blue = C_BLUE,
    )
}

/// One `zhis list` line: id, duration, relative time, then the command.
pub fn format_row(row: &Row, now: i64) -> String {
    let e = &row.entry;
    let count = if row.count > 1 {
        format!("{}{} ×{}", C_RESET, C_DIM, row.count)
    } else {
        String::new()
    };
    format_line(
        &row.id,
        &fmt_dur(e.m),
        &rel_time(e.t, now),
        &count,
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
        "...",
        "",
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
            (86_399_000, "23h59m"),
            (86_400_000, "1d00h"),
            (90_000_000, "1d01h"),
            (2_591_999_000, "29d23h"),
            (2_592_000_000, "1m00d"),
            (8_640_000_000, "3m10d"),
            (31_103_999_000, "11m29d"),
            (31_104_000_000, "1y00m"),
            (34_560_000_000, "1y01m"),
            (62_207_999_000, "1y11m"),
            (62_208_000_000, "2y00m"),
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
            count: 1,
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
    fn format_row_shows_the_collapse_count() {
        use crate::store::Entry;
        let folded = Row {
            entry: Entry {
                t: 0,
                d: "/d".into(),
                x: 0,
                c: "dup".into(),
                m: 0,
            },
            id: "0-1".into(),
            count: 15,
        };
        let out = format_row(&folded, 0);
        let cmd = out.find("dup").unwrap();
        let cnt = out.find("×15").unwrap();
        assert!(cmd < cnt, "count must follow the command: {:?}", out);

        let single = Row {
            entry: Entry {
                t: 0,
                d: "/d".into(),
                x: 0,
                c: "dup".into(),
                m: 0,
            },
            id: "0-1".into(),
            count: 1,
        };
        assert!(
            !format_row(&single, 0).contains('×'),
            "single row shows a count"
        );
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
            count: 1,
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
        assert!(out.contains("..."), "not marked as running: {:?}", out);
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
        assert_eq!(rel_time(now - 31_449_599, now), "51w");
        assert_eq!(rel_time(now - 31_449_600, now), " 1y");
        assert_eq!(rel_time(now - 94_348_800, now), " 3y");
    }
}
