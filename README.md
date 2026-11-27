# ait — an agent-first issue tracker

`ait` is one small Rust program that gives a team of AI agents (and their
owner) a shared issue tracker:

- a **CLI** built for agents: JSON on every command, structured errors and
  exit codes, `ait next --claim` to atomically pick the next ready issue,
  leases with generation numbers so a crashed agent can never overwrite its
  successor, handoff notes, acceptance criteria and evidence;
- a **local dashboard** (`ait serve`, 127.0.0.1 only): kanban board, list
  view with search and bulk changes, releases view, roadmap, flow statistics
  and an activity feed;
- a **SQLite database** kept inside `.git/ait/` of the repository it tracks,
  so it is shared by every worktree and can never be committed.

No installer and no runtime: copy `ait.exe` and `default.cfg` into a
repository and run `ait init`.

## Where to read next

| You want to | Read |
|---|---|
| Install it in a repository (written for agents) | [tools/issue-tracker/INSTALL.md](tools/issue-tracker/INSTALL.md) |
| Every command, the dashboard, import/export | [tools/issue-tracker/README.md](tools/issue-tracker/README.md) |
| Build a UI on top of it (HTTP + JSON contract) | [tools/issue-tracker/API.md](tools/issue-tracker/API.md) |
| Understand the design and its decisions | [DesignIteration6.md](DesignIteration6.md) |
| Work on the code (rules for agents) | [AGENTS.md](AGENTS.md) |

## Building

```
cargo build --release
```

The result is `target/release/ait.exe`. Dependencies (`rusqlite` with bundled
SQLite, `serde`, `serde_json`) are vendored in `vendor/`, and the build runs
offline. Quality gate:

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```
