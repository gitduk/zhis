# zhis

A shell history tool for zsh. It records every command with its working
directory, exit status, and duration. It replaces the zsh history file as the
persistent store.

zhis is a Rust reimplementation of [overflowy/zhist](https://github.com/overflowy/zhist)
(originally Go). The storage format, entry IDs, CLI, and `init` script are
byte-for-byte compatible with the original; the two binaries can share a
history file.

## Purpose

Native zsh history stores a command and a timestamp, nothing else. zhis stores
context: where you ran a command, whether it failed, and how long it took. The
fzf picker uses that context. Failed commands show in red, and each entry shows
its duration. One key toggles between global history and the current
directory's history.

## Install

Build from source:

```sh
cargo install --path .
```

or run `cargo build --release` and put `target/release/zhis` on your `PATH`.

Requires [fzf](https://github.com/junegunn/fzf) 0.45 or newer for the picker.

## Setup

Add to `.zshrc`:

```zsh
eval "$(zhis init)"
```

Import existing history once:

```sh
zhis import ~/.zsh_history
```

Imported entries have no directory, exit status, or duration. They show a
blank directory and never render red.

## Keys

| Key                | Action                                            |
| ------------------ | ------------------------------------------------- |
| `ctrl-r`           | Open the picker, searching for what is on the line |
| `up` / `down`      | Open the picker on an empty line; otherwise step line history |
| `ctrl-g`           | Toggle global / current-directory history         |
| `ctrl-u`           | Toggle the dedupe view (on by default)            |
| `ctrl-d`           | Delete the selected entry (no-op on a running one) |
| `ctrl-x`           | Delete all entries with the same command          |
| `ctrl-/`           | Turn the preview off, or back to automatic (remembered) |
| `tab`              | Accept and leave the command on the line          |

Pass `-no-arrow-binds` to `zhis init` to keep the default up/down behavior.
Only `ctrl-r` opens the picker then.

The picker opens on the current directory's history; `ctrl-g` widens it to
every directory and back. Which mode you are in is shown at the left of the
match counter.

The preview appears only when the row cannot show the whole command — because
it is too wide for the terminal, or because it has more than one line — and it
is exactly as tall as that command needs. `ctrl-/` turns it off entirely for
rows that would otherwise get one.

## Ignoring commands

Define a `HIST_EXCLUDE` array in `.zshrc` to keep commands out of the store.
zhis skips a command when its first word is in the array.

```zsh
HIST_EXCLUDE=(cd ls clear pwd exit)
```

- Matching is exact and case-sensitive, on the first word only. `ls` skips
  `ls -la` but not `lsd`.
- For multiline commands the first word of the first line decides.
- The hook reads the array at record time. Change it in a running shell and
  the change applies to the next command.
- A leading space also skips recording, for one-off exclusions.

## Running commands

zhis records a command from `precmd`, which zsh runs only after the command
returns. A long-running foreground command — a dev server, `tail -f`, `ssh` —
would therefore be missing from every other shell's history until it exited.

`preexec` now also writes a short-lived record, so the command shows in the
picker while it runs, marked `...` with a live elapsed time:

```
@31337  4.0s ... uv run uvicorn main:app --reload
```

Selecting one puts the command on the line like any other entry. `ctrl-d` and
`ctrl-x` do not apply: the record disappears on its own when the command ends,
at which point the real entry lands in `history.jsonl` with its exit status and
duration, keeping the timestamp it started with, so the row does not jump. Running commands ignore `ZHIS_LIST_LIMIT` — there are only ever as
many as you have shells.

Records live in `~/.local/share/zhis/inflight/<pid>.json`, one per shell, and
`history.jsonl` is untouched by them. A shell killed mid-command (`kill -9`)
leaves its record behind; the next `zhis list` notices the pid is gone and
sweeps it. The one gap: if the operating system reassigns that exact pid before
any sweep runs, the stale record survives until the sweep sees it die again.

Commands excluded from history — a leading space, or a first word in
`HIST_EXCLUDE` — are never written here either.

## Repeated commands

Repeated runs each get a row in the store — exit status and duration included.
The picker collapses each consecutive run of the same command to its newest
entry by default; `ctrl-u` toggles back to the full, faithful view.

- A run is a sequence of identical command text; exit status and duration do
  not break it, so the row shown is the run's most recent attempt.
- Only consecutive repeats collapse. The same command separated by other
  commands still shows every occurrence.
- With `ZHIS_LIST_LIMIT`, the dedupe view reads back far enough to show that
  many distinct commands; the limit caps the collapsed rows, not the raw
  entries behind them.
- The store is untouched; toggle off to see every entry.

## Large histories

`zhis list` reads and parses the whole history, so the picker's cost grows with
it. At one million entries that is about 0.7s and 300MB per `ctrl-r`, on top of
what fzf itself needs to ingest the rows.

`-limit N` reads backwards from the end of the file and stops after N entries,
which the picker will use if `ZHIS_LIST_LIMIT` is set:

```zsh
ZHIS_LIST_LIMIT=5000
```

At one million entries, `-limit 5000` drops that same query to under 10ms and
under 4MB. It also caps what a `ctrl-d`/`ctrl-x` reload re-reads.

It is off by default because it does hide history: nothing past the newest N
entries is searchable. Non-numeric values are ignored with a warning.

One subtlety. The limit selects by position in the file and only then sorts by
time, so the two orders can disagree. A `zhis import` writes old entries at the
end of the file, and an entry is timestamped when its command started, so a
command that ran for an hour lands in the file after commands that started —
and finished — while it was still running. If you have just imported and want
the oldest entries visible, leave the limit unset.

## Recommended zsh history settings

zhis owns persistence. Keep native history in memory only, for line stepping
and `!` expansion within a session.

```zsh
unset HISTFILE  # macOS /etc/zshrc sets it; unset stops zsh reading or writing the file
HISTSIZE=100000 # In-memory events for up-arrow and ! expansion
SAVEHIST=0      # Never write a history file

setopt BANG_HIST            # Treat the '!' character specially during expansion
setopt HIST_IGNORE_DUPS     # Don't record an entry that was just recorded again
setopt HIST_IGNORE_ALL_DUPS # Delete old recorded entry if new entry is a duplicate
setopt HIST_FIND_NO_DUPS    # Do not display a line previously found
setopt HIST_IGNORE_SPACE    # Don't record an entry starting with a space
setopt HIST_REDUCE_BLANKS   # Remove superfluous blanks before recording entry
setopt HIST_VERIFY          # Do not execute immediately upon history expansion
```

Do not set `SHARE_HISTORY`, `INC_APPEND_HISTORY`, or `EXTENDED_HISTORY`. They
only affect the history file, which zhis replaces.

Compatibility notes:

- Commands with a leading space are not recorded. This matches `HIST_IGNORE_SPACE`.
- `eval "$(zhis init)"` must run after plugins that bind `ctrl-r` or the
  arrow keys (atuin, zsh-history-substring-search, prompt pickers). The last
  bind wins.
- The record hook prepends itself to `precmd_functions` and passes `$?`
  through. Prompts that read the exit status in their own precmd keep working.

## CLI

What you run yourself:

```
zhis init [-no-arrow-binds]  Print the zsh integration script
zhis list [-dir D] [-limit N] [-uniq]  Print entries, newest first
zhis import FILE          Import a zsh EXTENDED_HISTORY file
zhis import-jsonl [FILE]  Import newline-delimited JSON entries (stdin if omitted)
```

What the hooks and the picker run for you. Nothing stops you from calling
these, and a bash or fish integration would be written against exactly them,
but typing one by hand is not a thing you should need to do:

```
zhis begin -pid N [-dir D] [-ts N]  Mark a command as started; command from stdin
zhis add -dir D -exit N [-ms N] [-ts N] [-pid N]  Append an entry; command from stdin
zhis get -id ID           Print the full command for an entry
zhis delete -id ID [-all] Delete an entry, or all entries with its command
```

## Storage

Entries are appended as JSON lines to `~/.local/share/zhis/history.jsonl`
(`ZHIS_FILE` overrides the path). Each entry:

```json
{"i":41,"t":1720000000,"d":"/home/you/src","x":0,"c":"cargo build","m":8123}
```

Commands still running live outside this file, one per shell, in
`~/.local/share/zhis/inflight/<pid>.json` (`{"t":..,"d":..,"c":..}` — no exit
status or duration, because there is not one yet).

- `i` — sequence number, unique and never reused
- `t` — unix timestamp
- `d` — working directory ("" if unknown)
- `x` — exit status (-1 if unknown)
- `c` — full command, may contain newlines
- `m` — duration in milliseconds, omitted when unknown

Entry IDs are `offset-base36` and the entry's sequence number in base 36. The
offset lets the picker seek straight to an entry; the sequence number is what
detects a stale ID, since a delete rewrites the file and shifts every later
offset while sequence numbers stay put. A `get` that lands on a mismatched
sequence number falls back to a scan.

Two sidecar files live beside the history: `history.jsonl.seq` holds the last
sequence number assigned, and `history.jsonl.lock` is the flock target. Back up
`.seq` along with the history — losing it is recoverable (the counter is
rebuilt from the last line) but losing only the history is not.

Appends and rewrites are serialized with `flock`; deletes rewrite the file
atomically (temp file + fsync + rename). The history file is created 0600, and
an append tightens group/other permissions if it finds them loosened.

## License

MIT
