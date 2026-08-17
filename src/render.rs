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
        format!("{:>2}s ago", ago)
    } else if ago < 3600 {
        format!("{:>2}m ago", ago / 60)
    } else if ago < 86400 {
        format!("{:>2}h ago", ago / 3600)
    } else if ago < 604800 {
        format!("{:>2}d ago", ago / 86400)
    } else {
        format!("{:>2}w ago", ago / 604800)
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

/// One `zhis list` line: id, duration, relative time, then the command
/// (multiline commands truncated to their first line, marked with "⏎").
pub fn format_row(row: &Row, now: i64) -> String {
    let e = &row.entry;
    let disp = match e.c.find('\n') {
        Some(i) => format!("{} ⏎", &e.c[..i]),
        None => e.c.clone(),
    };
    let col = if e.x > 0 { C_RED } else { "" };
    // FIELD_DELIM is invisible on screen, so each field's own trailing space
    // keeps a visible gap (fzf --with-nth reassembles using that same byte).
    format!(
        "{id}{d}{dim}{dur:>7} {reset}{d}{blue}{ago:>8} {reset}{d}{col}{disp}{reset}",
        id = row.id,
        d = FIELD_DELIM,
        dim = C_DIM,
        dur = fmt_dur(e.m),
        reset = C_RESET,
        blue = C_BLUE,
        ago = rel_time(e.t, now),
        col = col,
        disp = disp,
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
    fn rel_time_cases() {
        let now = 1_000_000;
        assert_eq!(rel_time(now - 5, now), " 5s ago");
        assert_eq!(rel_time(now - 59, now), "59s ago");
        assert_eq!(rel_time(now - 60, now), " 1m ago");
        assert_eq!(rel_time(now - 3599, now), "59m ago");
        assert_eq!(rel_time(now - 3600, now), " 1h ago");
        assert_eq!(rel_time(now - 86_399, now), "23h ago");
        assert_eq!(rel_time(now - 86_400, now), " 1d ago");
        assert_eq!(rel_time(now - 604_799, now), " 6d ago");
        assert_eq!(rel_time(now - 604_800, now), " 1w ago");
    }
}
