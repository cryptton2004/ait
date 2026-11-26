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

**Installing `ait` in another repository?** Follow [INSTALL.md](INSTALL.md):
where to copy the files, what to put in `default.cfg`, the block to add to
that repo's AGENTS.md, and a smoke test.

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
- **Approvals (owner only):** set `approvals = approvals.json` (a file) or
  `approvals = git:main:docs/approvals.json` (the file as committed on `main`,
  so an uncommitted edit has no effect). Once set, `next` only offers issues an
  approval covers, and a missing file approves nothing. A `build` approval
  offers `ready` issues to `next`; a `frame` approval offers `idea`/`planned`
  issues to `next --frame` (write up, don't build). `ait` never writes this
  file, so protect it from agents with an `ask` rule in `.claude/settings.json`.
- **Agent tiers:** `agent.<tier> = <model>` lines in `default.cfg` (shipped:
  light, standard, deep). An issue's `--agent deep` then shows as
  `agent_model: opus` in `show`, `next` and claims.
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
ait done CORE-1-10 --summary "what was delivered" --gen GEN --ran "sonnet, effort high"
```

If an agent gets `STALE_GENERATION` or `NOT_CLAIMANT`, its lease expired and
someone else took the issue over. It must stop working on that issue. Notes,
handoffs, state changes and edits from anyone but the live claimant are
refused unless they pass `--force` (the owner's escape hatch).

## Commands

Every command accepts `--json` (machine output), `--as <session>`,
`--db <file>` and `--config <file>`. Ids are case-insensitive, and old
`legacy` names are accepted anywhere an id is.

| Command | What it does |
|---|---|
| `add <PREFIX\|PREFIX-N\|ID> "title"` | New issue. With a bare prefix the id follows the hierarchy: a feature, milestone or release opens a new group (`CORE-2-00`); an issue with `--parent` takes the next number in its parent's group (`CORE-2-01`); any other issue takes the next number in the newest group that is not a feature's (or opens one). `CORE-2` picks the next number in group 2, `CORE-1-05` exactly that id. Options: `--body`/`--body-file` (Markdown), `--after ID` (next free number after it, placed after it in the order), `--state` (default `ready`), `--reason`, `--size`, `--spec`, `--parent`, `--legacy`, `--needs A,B` (or `A:in_review` to wait only until A reaches that state), `--discovered-from ID`, `--criteria "one per line"` (acceptance criteria), `--type task\|feature\|milestone\|release\|review` (default task; a review needs `--parent`), `--priority P0..P4` (default P2), `--agent <tier>`, `--effort low\|medium\|high\|xhigh\|max`, `--reviewer <name>`, `--release <release or milestone id>`, `--labels a,b` (lowercase words), `--due YYYY-MM-DD` (expected date; `--due ""` clears it). After adding, the reply lists open issues with a similar title (`possible_duplicates`) so you can drop a duplicate |
| `edit <ID>` | Change any field `add` takes. `--version N` refuses the change if someone else changed the issue first. A claimed issue can only be edited by its claimant (`--gen N`), or with `--force` |
| `state <ID> <state> [--reason r]` | `idea`, `planned`, `ready`, `blocked`, `decision`, `dropped`, `in_review`. `blocked`, `decision` and `dropped` need a reason. `in_review` submits the work for review (all acceptance criteria checked, gives the claim back). Use `done` to close an issue and `ship` to release it. Finished work is only reopened with `--force`; a claimed issue only changes by its claimant or with `--force` |
| `move <ID> --before/--after ID \| --top \| --bottom` | Change the order (the order is what `next` follows) |
| `link <ID> needs\|blocked-by\|relates\|discovered-from <ID> [--until state]` / `unlink …` | Links. `needs` (alias `blocked-by`) keeps the issue waiting until the other one is done, or reaches the `--until` state (`planned`, `ready`, `in_review`, `done`, `released`). Cycles are refused |
| `criteria <ID>` | Acceptance criteria. No option lists them; `--add "text"` (one per line), `--check N [--evidence "test name, commit or link"]`, `--uncheck N`, `--remove N`. Same claim rules as `edit` |
| `delete <ID> [--force]` | Deletes an issue. Refused if it is claimed or other issues need it, unless `--force`. The id is retired and never reused |
| `show <ID> [--notes N]` | Issue, links, claim, readiness with reasons, latest handoff, recent notes |
| `list` | `--state a,b`, `--prefix P`, `--ready`, `--claimed`, `--stale`, `--all` (include done and dropped), `--limit N`, `--type`, `--priority P0,P1`, `--agent tier`, `--parent ID`, `--release ID`, `--tree` (indented by parent) |
| `ship <ID> [--ref r] [--force]` | Release a release, milestone or feature: every member must be done or dropped, and every done task merged into `main` (with the `Issue-ID` trailer, or in `git_baseline`) and, when `origin/main` exists, pushed. Marks the container and its done members `released`. `--force` skips the git checks |
| `roadmap [--all]` | Releases, milestones and features, each with done/total, percent, in progress, the next offered member, open decisions and blocked members. A container's members are its children (`parent`) and everything whose `release` is it, recursively |
| `next` | What would be offered, by priority then order (`--prefix`, `--limit`, `--agent`, `--priority`, `--type`; tasks only unless `--type` is given). `--claim` claims the top one. `--take-stale` also takes over expired claims. `--frame` offers `idea`/`planned` issues with a frame approval instead. `--explain ID` says why an issue isn't offered and names the approval cause (`FRAME_ONLY`, `EXPIRED`, `EXCLUDED`, `NOT_STARTED`, `NOT_APPROVED`, `NO_APPROVALS_FILE`) |
| `claim <ID> [--take] [--lease 2d]` | Claim one issue. `--take` takes over a claim whose lease expired, and returns the previous handoff. `--lease` gives this claim its own lease length (1m to 30d, for example for a decision that needs meetings); renewals keep that length. `next --claim` takes `--lease` too |
| `heartbeat [<ID>]` | Renews your live leases. Any `ait` call by you renews them too, at most once a minute. An expired lease is not revived: claim the issue again |
| `verify <ID> --gen N` | Succeeds only if you still hold generation N |
| `release <ID> [--gen N] [--force]` | Give an issue back. Only the claimant can, unless `--force` |
| `done <ID> --summary "…" --gen N [--ran "…"] [--force]` | Close an issue and free its claim. Needs your live claim and its generation; `idea`, `decision` and `dropped` issues and unclaimed issues need `--force`. `--ran` records what actually did the work |
| `note <ID> "text"` / `--text-file f` | Append a note. Notes are never edited |
| `handoff <ID> ["text" \| --from-file f]` | Save a "how to resume" snapshot. Old handoffs stay in history. With no text, shows the latest |
| `brief <ID> [--out f]` | A self-contained Markdown brief for a cloud session. Use `--out` rather than `>` in PowerShell, which would re-encode the file |
| `stats [--since 7d]` | Flow stats from the event log: per column how many issues passed and how long they stayed (median, p90), how many sit there now and the oldest; cycle time (first claim to done) and lead time (added to done) |
| `board [--limit N]` | One screen: in progress, stale, waiting (decision or blocked), ready, recently done |
| `activity` | Event feed: `--since 1h`, `--after SEQ`, `--issue ID`, `--limit N` |
| `serve [--port N] [--read-only]` | Dashboard at `http://127.0.0.1:7777/` (see below). Stop it with Ctrl+C |
| `check [--git] [--repo dir]` | Consistency check (ids, states, types, priorities, cycles, dangling links, duplicate legacy names). `--git` reads the repository from `repo =` (or `--repo`), not the current folder, and compares first-parent merge commits on `main` whose last paragraph carries `Issue-ID: <id>` lines: `DONE_NOT_MERGED` (done, not merged, not in `git_baseline`) and `MERGED_NOT_DONE` |
| `backup` | Backup now. A daily backup also happens automatically |
| `export [--out f]` / `import <f>` | Readable JSONL dump and restore (into an empty database). Import validates everything first and writes nothing if anything is wrong; `IMPORT_INVALID` lists every problem in `error.details.problems` |
| `import --issues <f> [--prefix-for feature=EPIC,…]` | Load an inventory, one JSON issue per line (see below), into an empty database |
| `init`, `guide`, `help`, `version` | |

**Readiness:** `next` offers an issue only when it is a task (or a review
whose parent is in review) in state `ready`, every `needs` issue has reached
its target state (`done` by default), a build approval covers it (if
approvals are used), its WIP limits have room, and nobody holds a claim on
it. A `dropped` prerequisite keeps blocking.

**Flow:** the board columns are, left to right, backlog (idea, planned),
ready, waiting (blocked, decision, or unmet needs), in progress (claimed),
in review, done and released.

- *Reviews:* add review issues with `--type review --parent ID` (for example
  code, security, legal). They become claimable only once the parent is
  `in_review`. The parent can be closed only when all its reviews are done;
  a reviewer who finds problems notes them on the review, releases it, and
  moves the parent back with `ait state <parent> ready --reason "review: ..."`.
- *Acceptance criteria:* every criterion must be checked before an issue goes
  to `in_review` or `done` (`--force` overrides, as the owner).
- *Containers roll up:* a feature, milestone or release moves to `in_review`
  by itself when all its children are in review or finished, and to `done`
  when all are finished (and its own reviews and criteria are done). It goes
  back to `planned` if a child is reopened. `state` and `done` report this in
  `rollup`.
- *Release:* once everything is done and merged, `ait ship <ID>` marks it
  `released`.
- *WIP limits:* `wip.in_progress`, `wip.in_review` and `wip.agent.<tier>` in
  `default.cfg`. A full column refuses new claims (or submissions) with
  `WIP_LIMIT`, which is retryable; `next` skips tiers that are full.

**Inventory format (`import --issues`):** one JSON object per line. Fields:
`title` (required), `id` or `legacy`, `type` (or `kind`), `body`, `state`
(default `ready`), `reason`, `parent`, `release`, `needs` and `relates`
(lists), `labels` (list, appended to the body), `size`, `spec`, `order`,
`priority` (default P2), `agent`, `effort`, `reviewer`, `done_summary`,
`source` (appended to the body). `parent`, `release`, `needs` and `relates`
may name an id or any record's `legacy` value. A record's id is its `id`, else
its `legacy` if that is a valid id, else the next free id under its type's
prefix (`--prefix-for`, default `task=TASK,feature=FEAT,milestone=MS,release=REL`).

**Leases and generations:** a claim lasts `lease_minutes` (default 60), and
every call by the claimant renews it. Each new claim of an issue gets the next
generation number. Writes that carry an old `--gen` are refused, so an agent
that times out and wakes up later cannot overwrite its successor's work.

## Dashboard

`ait serve` shows the board live (it reloads within about 2 seconds of any change):
columns, what `ait next` and `ait next --frame` would offer (the top card is
marked NEXT), the roadmap, the activity feed, and each issue's details.

From the dashboard you can also add issues, edit any field (including the
description), change the state, close an issue as done, add notes, and rank
issues by dragging a card onto another (cards sort by priority, then rank, so
dropping onto a card of another priority also takes that priority). Dragging a
card onto a column changes its state and asks for what the command needs (a
reason for Waiting, a summary for Done).

Every change runs the same `ait` command the CLI would (`add`, `edit`,
`state`, `move`, `done`, `note`, `link`, `unlink`, `ship`, `criteria`, `claim`, `release`, `delete`), recorded as the session
`dashboard`, so the same checks apply. When the CLI would ask for `--force`
(an agent's claim, closing without a claim, reopening done work), the
dashboard shows the refusal and an explicit "Override (--force)" button.
Cards show stale claims with a badge (and a filter), column headers show
WIP "live / limit", issue details have the acceptance checklist, and the
Flow panel shows `ait stats` for 7 days, 30 days or all time. Dropping a card
on In review submits it; dropping it on Released runs `ship`.
Dropping a card on In progress claims it for you (session `dashboard`) with a lease you choose (60m prefilled, for example 2d); dropping it back on Ready releases it, and Done closes it with that claim. Imports and backups stay on the command line.

The header switches between three views (remembered per browser) and has a
search box that filters the board and the list:

- **Board:** the columns above.
- **List:** every issue, dropped ones included, 50 at a time with Load more.
  Sort by any column, filter by state (or "open") and label, view, edit or
  delete a row, and tick rows to change priority, state, parent or labels for
  all of them at once.
- **Releases:** each release with its expected date (overdue ones are
  flagged) and progress, and a table of its features and issues (id, title,
  state, assignee), plus the features not in a release yet.

Checking a criterion asks for optional evidence; adding an issue warns about
similar open titles with links to them.

Safety: the server listens on 127.0.0.1 only and refuses other Host names.
Each start creates a random token that only the dashboard page carries;
writes without it, from another site (`Origin`, `Sec-Fetch-Site`), or not
sent as JSON are refused, and options that read files or switch database or
identity are refused. `ait serve --read-only` turns writes off.

**Using it from another local UI:** list that page's origin in
`allowed_origins` (for example `http://localhost:5173`). It may then call the
API with CORS, get the token from `/api/token`, and show the dashboard in an
iframe; every other origin stays refused. The endpoints and every `--json`
shape are a versioned contract in [API.md](API.md).

## Errors and exit codes

With `--json`, an error is printed on stdout as
`{"error":{"code":"ALREADY_CLAIMED","message":"…","retryable":false}}`.
Without `--json`, a one-line message goes to stderr.

| Exit | Meaning | Codes |
|---|---|---|
| 0 | ok | |
| 1 | internal / database / disk error | `DB_ERROR`, `IO_ERROR`, `INTERNAL` |
| 2 | bad input or setup | `USAGE`, `INVALID_ID`, `INVALID_INPUT`, `INVALID_STATE`, `UNKNOWN_PREFIX`, `CONFIG`, `NO_SESSION`, `NO_DB`, `IMPORT_INVALID` |
| 3 | not found | `NOT_FOUND` |
| 4 | conflict | `ALREADY_CLAIMED`, `CLAIM_STALE`, `STALE_GENERATION`, `NOT_CLAIMANT`, `NOT_CLAIMED`, `VERSION_CONFLICT`, `CYCLE`, `IN_USE`, `ID_TAKEN`, `GROUP_FULL`, `ALREADY_DONE`, `NOT_EMPTY`, `NOT_SHIPPABLE`, `REVIEWS_OPEN`, `REVIEW_NOT_STARTED`, `WIP_LIMIT` (retryable), `CRITERIA_OPEN` |
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
| `src/roadmap.rs` | rollups and `ait roadmap` |
| `src/roadmap.rs` (cont.), `src/criteria.rs`, `src/stats.rs` | `ship` and auto-rollup, acceptance criteria, flow stats |
