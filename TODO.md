# TODO

Handoff notes. Written after a round of work on 2026-08-17; the sections below
are self-contained, assume no prior conversation context.

## This round

Closed out the previous round's open work:

1. **Initial commit made** (`1d75a17`), so a git baseline exists and
   `/security-review` no longer dies on `git diff origin/HEAD...`.
2. **Security review** of the two surfaces the prior round touched:
   `Store::append`'s permission tightening and `ZHIS_LIST_LIMIT` flowing into
   fzf's command strings. Both are clean — `set_permissions` clears group/other
   on the already-open handle (no TOCTOU), and the `<->` digits guard in
   init.zsh rejects anything but a bare integer before it reaches the
   reload/toggle strings.
3. **Small cleanups** (commit `976aa50`): radix36 now builds its String from
   chars (`map(char::from).collect()`) instead of `from_utf8().unwrap()`;
   `Store::delete` and `Store::append` got the two missing "why" comments; and
   `format_row` strips control bytes from the displayed command so a pasted
   `\x1f` or ANSI escape can no longer mint phantom fzf columns or recolor the
   picker (`zhis get` still returns the command verbatim). Tests went 40 -> 41.

`cargo clippy --all-targets -- -D warnings` clean; `cargo test` green (41).

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
