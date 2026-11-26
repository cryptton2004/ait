# ait: agent-first issue tracker

`ait` is one small program (`ait.exe`) that keeps your issues in a local SQLite
database. Agents call it from the command line: each call opens the database,
does one short transaction and exits. That takes about 15 ms on this PC, and
most of that is Windows starting the process.

It solves three problems, in this order:

1. **Two agents grabbing the same task.** `ait next --claim` picks and claims
   an issue in one transaction, so exactly one agent wins.
2. **Agents losing context.** Notes, handoffs and `ait show` give a new
   agent everything it needs to resume.
3. **Seeing progress.** `ait board`, `ait activity` and the `ait serve`
   dashboard.

The design is in `DesignIteration6.md` at the repository root. This README
describes what is actually built.

## Build

```
cargo build --release
copy target\release\ait.exe tools\issue-tracker\ait.exe
```

You need Rust (stable, MSVC) and the Visual Studio C++ build tools. The
libraries are vendored in `vendor/`, so the build works offline.
`ait.exe` is git-ignored: build it, don't commit it.

## Setup

- **Database:** by default `<repo>/.git/ait/tracker.db`. Every worktree of the
  repository finds the same file, and it is never committed. Change it with `db =`
  in `default.cfg`, `--db <file>`, or the `AIT_DB` environment variable.
- **Config:** `default.cfg` next to `ait.exe` (or `--config <file>` /
  `AIT_CONFIG`). Every key is explained in the file.
- **Approvals (owner only):** copy `approvals.example.json` to
  `approvals.json` to turn approvals on. Once the file exists, `next` only
  offers issues an approval covers. `ait` never writes this file, so protect
  it from agents with an `ask` rule in `.claude/settings.json`.
- **Identity:** every agent sets `AIT_SESSION=<unique name>` (or passes
  `--as <name>`). Claims belong to sessions.

## Agent workflow

`ait guide` prints this cheat sheet, ready to paste into an AGENTS.md:

```
set AIT_SESSION=agent-3
ait next --claim --json                 # -> issue, claim.generation (GEN), latest_handoff
ait show CORE-1-10 --json               # everything needed to resume
ait note CORE-1-10 "decided X because Y" --gen GEN
ait handoff CORE-1-10 "done: ... remaining: ... branch: ... last commit: ..." --gen GEN
ait add CORE "found bug in Z" --discovered-from CORE-1-10
ait verify CORE-1-10 --gen GEN          # right before merging
ait done CORE-1-10 --summary "what was delivered" --gen GEN
```

If an agent gets `STALE_GENERATION` or `NOT_CLAIMANT`, its lease expired and
someone else took the issue over. It must stop working on that issue.

## Commands

Every command accepts `--json` (machine output), `--as <session>`,
`--db <file>` and `--config <file>`. Ids are case-insensitive, and old
`legacy` names are accepted anywhere an id is.

| Command | What it does |
|---|---|
| `add <PREFIX\|PREFIX-N\|ID> "title"` | New issue. `CORE` takes the next number in the newest group, `CORE-2` the next number in group 2, `CORE-1-05` exactly that id. Options: `--body`/`--body-file` (Markdown), `--after ID` (next free number after it, placed after it in the order), `--state` (default `ready`), `--reason`, `--size`, `--spec`, `--parent`, `--legacy`, `--needs A,B`, `--discovered-from ID` |
| `edit <ID>` | Change `--title`, `--body`/`--body-file`, `--size`, `--spec`, `--parent`, `--legacy`. `--version N` refuses the change if someone else changed the issue first |
| `state <ID> <state> [--reason r]` | `idea`, `planned`, `ready`, `blocked`, `decision`, `dropped`. The last three need a reason. Use `done` to close an issue |
| `move <ID> --before/--after ID \| --top \| --bottom` | Change the order (the order is what `next` follows) |
| `link <ID> needs\|relates\|discovered-from <ID>` / `unlink …` | Links. `needs` blocks readiness, and cycles are refused |
| `delete <ID> [--force]` | Deletes an issue. Refused if it is claimed or other issues need it, unless `--force`. The id is retired and never reused |
| `show <ID> [--notes N]` | Issue, links, claim, readiness with reasons, latest handoff, recent notes |
| `list` | `--state a,b`, `--prefix P`, `--ready`, `--claimed`, `--stale`, `--all` (include done and dropped), `--limit N` |
| `next` | What would be offered (`--prefix`, `--limit`). `--claim` claims the top one. `--take-stale` also takes over expired claims. `--explain ID` says why an issue isn't offered |
| `claim <ID> [--take]` | Claim one issue. `--take` takes over a claim whose lease expired, and returns the previous handoff |
| `heartbeat [<ID>]` | Renews your leases. Any `ait` call by you renews them too, at most once a minute |
| `verify <ID> --gen N` | Succeeds only if you still hold generation N |
| `release <ID> [--gen N] [--force]` | Give an issue back. Only the claimant can, unless `--force` |
| `done <ID> --summary "…" [--gen N] [--force]` | Close an issue and free its claim |
| `note <ID> "text"` / `--text-file f` | Append a note. Notes are never edited |
| `handoff <ID> ["text" \| --from-file f]` | Save a "how to resume" snapshot. Old handoffs stay in history. With no text, shows the latest |
| `brief <ID> [--out f]` | A self-contained Markdown brief for a cloud session. Use `--out` rather than `>` in PowerShell, which would re-encode the file |
| `board [--limit N]` | One screen: in progress, stale, waiting (decision or blocked), ready, recently done |
| `activity` | Event feed: `--since 1h`, `--after SEQ`, `--issue ID`, `--limit N` |
| `serve [--port N]` | Read-only dashboard at `http://127.0.0.1:7777/`. Stop it with Ctrl+C |
| `check [--git]` | Consistency check. `--git` flags issues marked done with no commit on `main` carrying `Issue-ID: <id>` |
| `backup` | Backup now. A daily backup also happens automatically |
| `export [--out f]` / `import <f>` | Readable JSONL dump and restore (into an empty database) |
| `init`, `guide`, `help`, `version` | |

**Readiness:** `next` offers an issue only when its state is `ready`, every
`needs` issue is `done`, an approval covers it (if approvals are used), and
nobody holds a claim on it. A `dropped` prerequisite keeps blocking.

**Leases and generations:** a claim lasts `lease_minutes` (default 60), and
every call by the claimant renews it. Each new claim of an issue gets the next
generation number. Writes that carry an old `--gen` are refused, so an agent
that times out and wakes up later cannot overwrite its successor's work.

## Errors and exit codes

With `--json`, an error is printed on stdout as
`{"error":{"code":"ALREADY_CLAIMED","message":"…","retryable":false}}`.
Without `--json`, a one-line message goes to stderr.

| Exit | Meaning | Codes |
|---|---|---|
| 0 | ok | |
| 1 | internal / database / disk error | `DB_ERROR`, `IO_ERROR`, `INTERNAL` |
| 2 | bad input or setup | `USAGE`, `INVALID_ID`, `INVALID_INPUT`, `INVALID_STATE`, `UNKNOWN_PREFIX`, `CONFIG`, `NO_SESSION`, `NO_DB` |
| 3 | not found | `NOT_FOUND` |
| 4 | conflict | `ALREADY_CLAIMED`, `CLAIM_STALE`, `STALE_GENERATION`, `NOT_CLAIMANT`, `NOT_CLAIMED`, `VERSION_CONFLICT`, `CYCLE`, `IN_USE`, `ID_TAKEN`, `GROUP_FULL`, `ALREADY_DONE`, `NOT_EMPTY` |
| 5 | busy, retry shortly | `BUSY` |
| 6 | nothing ready to claim | `NO_WORK` |
| 7 | `check` found problems | |

## Safety of the data

- SQLite in WAL mode, 5 s busy timeout, and every write in `BEGIN IMMEDIATE`.
  Ten agents writing at once just queue for a few milliseconds.
- The first `ait` call each day writes `backups/tracker-YYYY-MM-DD.db` next to
  the database and keeps the newest 14. To copy the database by hand, use
  `ait backup` or `ait export`. The `.db` file alone can be missing the latest
  writes, which live in `tracker.db-wal` until SQLite folds them in.
- `ait export --out snapshot.jsonl` writes a sorted, readable dump. Exporting,
  importing and exporting again gives a byte-for-byte identical file (this is
  tested).
- Schema changes happen only through numbered migrations, and an older `ait`
  refuses a newer database.

## Cloud sessions (rare)

1. On the PC: `ait claim CORE-1-10 --as cloud-1`
2. `ait brief CORE-1-10 --out BRIEF.md`, then paste it into the cloud prompt
   or commit it on the session's branch.
3. The cloud session writes `HANDOFF.md` on its branch.
4. Back on the PC: `ait handoff CORE-1-10 --as cloud-1 --from-file HANDOFF.md`,
   then `ait done … --as cloud-1` or `ait release …`.

## For developers (agents)

Rules are in `AGENTS.md`. The quality gate, run from the repository root:

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Integration tests in `tests/` run the real binary against temporary databases
under `target/tmp/`. `AIT_NOW_MS` overrides the clock, and exists only so tests
can expire leases without sleeping.

| File | Contents |
|---|---|
| `src/main.rs` | argument handling and command dispatch |
| `src/db.rs` | opening, pragmas, migrations, transactions, sessions, events |
| `src/ids.rs` | id rule and allocation |
| `src/issues.rs` | add, edit, state, move, link, delete, show, list |
| `src/claims.rs` | readiness, next, claim, release, done, heartbeat, verify |
| `src/notes.rs` | notes, handoffs, brief |
| `src/board.rs` | board and activity |
| `src/check.rs`, `src/backup.rs`, `src/exchange.rs` | check, backups, export/import |
| `src/serve.rs`, `src/dashboard.html` | local dashboard |
| `src/config.rs`, `src/approvals.rs` | default.cfg and approvals |
