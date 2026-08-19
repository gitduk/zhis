# TODO

Handoff notes. Written after a round of work on 2026-08-19; the sections below
are self-contained, assume no prior conversation context.

## This round

Added in-flight command tracking, so a long-running foreground command is
visible to other shells before it finishes.

The root cause: the record hook ran only from `precmd`, which zsh fires after a
command returns. `uv run uvicorn --reload` occupying a terminal was therefore
absent from every other shell's ctrl-r until killed.

1. **`src/inflight.rs`** (new). One JSON record per shell at
   `<data_dir>/inflight/<pid>.json`, written by a new `zhis begin -pid`, cleared
   by `zhis add -pid`. No locking: a shell only ever writes the file its own pid
   names. Stale records (shell killed mid-command) are swept on read via
   `kill(pid, 0)`; `libc` was added as a dependency for that one call.
2. **Picker integration.** `zhis list` prints running rows above stored history
   via `render::format_running_row`, deliberately outside `-limit`. Ids use an
   `@<pid>` namespace that `store::parse_id` cannot parse, so the two id spaces
   cannot be confused. `cmd_get` routes to the in-flight store; `cmd_delete`
   returns early on an in-flight id (deliberate no-op — deleting the record
   would not stop the process).
3. **Exclusion moved to `preexec`.** The leading-space and `HIST_EXCLUDE` checks
   used to live in `precmd`. They now run in `preexec` and gate `zhis begin`
   too; otherwise a space-prefixed command would have been exposed to every
   picker for as long as it ran.
4. **ctrl-r / arrow keymap fix** (unrelated bug found in passing). Both binds
   used a bare `bindkey`, which only hits the current keymap — under
   `bindkey -v` that left vicmd's ctrl-r on whatever plugin bound it last,
   reading the zsh history file zhis replaces. ctrl-r is now bound in emacs,
   viins and vicmd; the arrows only in emacs and viins (in vicmd they are vi's
   own history motion).

`cargo clippy --all-targets -- -D warnings` clean; `cargo test` green (44 + 11).

## Durable decisions — do not redo

- **history.jsonl was deliberately not touched.** Three alternatives were
  evaluated and rejected: patch-lines merged on read (breaks `-limit`'s reverse
  window, and adds a field, breaking Go-`zhist` storage compatibility);
  in-place line rewrite (JSON forbids leading zeros, so no fixed-width
  placeholder exists, and under concurrent shells the target is not reliably the
  last line); moving the record point wholly to `preexec` (loses exit status and
  duration, which are the point of the tool).
- **A status field on `Row` was rejected.** It would push an in-flight concept
  into `Store`'s type. The duplication it was meant to remove is gone anyway:
  `format_row` and `format_running_row` now share `render::format_line`.
- **A central id dispatcher was rejected.** Only `get` and `delete` consume ids,
  and one of them intentionally no-ops; an enum dispatcher is more machinery
  than two call sites justify.
- **Backgrounding `zhis begin` (`&!`) was rejected.** It races: `begin` could
  land after `add` clears, leaving a permanent phantom "running" row.
- **Trimming `write_record`'s syscalls was rejected.** Measured, `zhis begin`
  costs ~2.3ms of which ~1.1ms is bare fork+exec; the 2-3 syscalls in question
  are ~10µs of that, and skipping the directory-mode check would weaken the
  0700 guarantee on a directory holding pasted secrets.

## Known limitation

pid reuse. A shell killed with `kill -9` leaves its record; the sweep removes it
once it sees the pid is dead. If the OS reassigns that exact pid before any
sweep runs, the stale record survives until the new process also exits. The
window needs a SIGKILL mid-command plus pid wraparound before any `zhis list`,
and the consequence is one wrong picker row. Accepted and documented in README.

## Not done

- **`/security-review` never ran.** It resolves its target as
  `git diff origin/HEAD...`, which sees only committed history, and this work is
  uncommitted. `refs/remotes/origin/HEAD` was also unset locally and was pointed
  at `origin/master` to unblock the tool. Re-run it after committing.

## Previous round (2026-08-17)

## Not done / to reconsider

- **`/code-review xhigh` still has no recorded result.** It was delegated to a
  subagent this round, but the subagent returned no output, so I re-did the
  review by hand instead. A manual line-by-line pass found no correctness bugs
  in `nth_line_start_from_end`, `trailing_line_size`, `read_tail`, the offset
  tracking, or the id round-trip. One minor observation left alone: `cmd_add`
  uses `trim_end_matches('\n')`, which strips *all* trailing newlines, not just
  the one `print -r` adds — a command whose text legitimately ends in newlines
  loses them. Probably intended (byte-compat with the original zhist); revisit
  only if fidelity matters.
- **The two reverse scanners still coexist** (`trailing_line_size` and
  `nth_line_start_from_end`). Still deliberately not merged; see below.

## Durable decisions — do not redo

- Pre-filtering `dir` before full JSON decode was rejected (no headroom; adds
  escape-handling complexity).
- Hoisting `content_end`/the scan buffer out of `read_tail` was rejected
  (spares a syscall on a path that only runs for sparse `-dir` filters).
- A shared `test_dir()` helper in tests/cli.rs was rejected (wraps one call).
- `read_tail`'s `checked_mul(4)` overflow guard is unreachable today (window >
  line count returns `hit_start` first); kept as zero-cost defence, not because
  it fires.

## Environment notes

- `cargo` must run with `--offline`; plain network fetches hang.
- Formatting is owned by the git pre-commit hook — never run formatters by
  hand. `cargo clippy --all-targets -- -D warnings` and `cargo test` are the
  verification chain.

## Semantic caveat worth keeping in mind

`-limit N` selects the last N *lines* and only then sorts by time. zhis's own
appends agree (time order), but `zhis import` appends old entries at the end,
so `zhis list -limit 3` right after an import shows ancient entries and hides
recent commands. This is why the picker defaults to no limit; `ZHIS_LIST_LIMIT`
is opt-in. If it ever becomes default-on, this needs an index or a sorted
superset, not documentation.
