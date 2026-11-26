# ait API contract — version 1

This is the stable contract for programs that drive `ait`: agents using the
CLI with `--json`, and web pages (the built-in dashboard or a factory UI)
using the local HTTP API of `ait serve`. Everything here is covered by the
integration tests in `tests/`. Anything not written here (text output, the
dashboard's HTML, database tables) is internal and may change at any time.

- Contract version: **1** (`GET /api/version` → `"api": 1`)
- Database schema it was written against: **5**

## 1. Versioning rules

Within API version 1 we only make **additive** changes:

- new commands, endpoints, options, fields in an object, error codes, states;
- longer or reworded `message` texts.

Clients must therefore ignore fields they do not know, treat an unknown
error `code` by its exit code / HTTP status, and treat an unknown state as
"not finished". Anything else (removing or renaming a field, changing a
field's type or meaning, changing an exit code, changing what a guard
accepts) raises the version to 2, gets a note in this file, and the old
behaviour stays documented until it is removed.

Check compatibility at start-up with `GET /api/version` (HTTP) or
`ait version --json` (CLI: `{"version", "schema"}`).

## 2. Common shapes

**Timestamps** are UTC ISO-8601 strings (`"2026-11-26T08:00:00Z"`). A few
objects also carry the same instant as `*_ms` (milliseconds since 1970).
Missing values are `null`, never absent, inside the objects below.

**Issue ids** look like `CORE-1-00` (prefix, group, two-digit number).
Inputs accept a unique prefix of an id; outputs always use the full id.

**Issue object** (from `add`, `edit`, `state`, `show`, `claim`, `next`, `list`):

| field | type | meaning |
|---|---|---|
| `id` | string | full id |
| `title`, `body` | string | `body` is left out by `list` and `next` |
| `type` | string | `task`, `feature`, `milestone`, `release`, `review` |
| `state` | string | see states below |
| `priority` | string | `P0` (most urgent) .. `P4` |
| `order` | int | rank inside a priority; lower comes first (`ait move`) |
| `parent`, `release` | id or null | containers this issue belongs to |
| `agent`, `agent_model` | string or null | recommended tier and the model it maps to |
| `effort`, `size`, `spec`, `reviewer`, `reason`, `legacy` | string or null | planning fields |
| `summary`, `ran`, `done_at` | null until done | what was delivered, by which model |
| `released_at`, `release_ref` | null until shipped | when, and the git ref it shipped in |
| `version` | int | bumped on every change; pass `--version N` to `edit` for optimistic locking |
| `created_at`, `updated_at` | timestamp | |

`list` items also carry `claim` (claim object or null).

**States:** `idea`, `planned`, `ready`, `blocked`, `decision`, `dropped`,
`in_review`, `done`, `released`. Finished = `done` or `released`.
A claim is not a state: a claimed issue keeps its state (usually `ready`).

**Board columns** (dashboard and `stats`), left to right: `backlog`
(idea, planned), `ready`, `waiting` (blocked, decision, or unmet needs),
`in_progress` (claimed), `in_review`, `done`, `released`. Dropped issues are
not on the board.

**Claim object:** `issue`, `session`, `generation` (int, rises on every new
claim of the issue), `claimed_at`, `lease_until`, `lease_minutes` (this
claim's own lease length from `--lease`, or null for the configured
default), `status` (`live` or `stale`).

**Criterion object:** `n` (1-based position), `text`, `done` (bool),
`done_at`, `done_by`.

## 3. Errors and exit codes

Every failure has the same shape. With `--json` the CLI prints it on
**stdout**; the HTTP API returns it as the body.

```json
{"error":{"code":"ALREADY_CLAIMED","message":"CORE-1-00 is claimed by a1 (generation 1) until 2026-11-26T09:00:00Z","retryable":false}}
```

`details` is added when there is structured extra data (for example
`NOT_SHIPPABLE` → `{"problems": [...]}`, import problems). `retryable: true`
means the same call may succeed later without changes (`BUSY`, `WIP_LIMIT`,
`NO_WORK`).

| exit | HTTP | codes |
|---|---|---|
| 0 | 200 | success |
| 1 | 500 | `DB_ERROR`, `IO_ERROR`, any unlisted code |
| 2 | 400 | `USAGE`, `INVALID_ID`, `INVALID_INPUT`, `INVALID_STATE`, `UNKNOWN_PREFIX`, `CONFIG`, `NO_SESSION`, `NO_DB`, `INVALID_JSON`, `IMPORT_INVALID` |
| 3 | 404 | `NOT_FOUND` |
| 4 | 409 | `ALREADY_CLAIMED`, `CLAIM_STALE`, `STALE_GENERATION`, `NOT_CLAIMANT`, `NOT_CLAIMED`, `VERSION_CONFLICT`, `CYCLE`, `IN_USE`, `NOT_EMPTY`, `ID_TAKEN`, `GROUP_FULL`, `ALREADY_DONE`, `NOT_SHIPPABLE`, `REVIEWS_OPEN`, `REVIEW_NOT_STARTED`, `WIP_LIMIT`, `CRITERIA_OPEN` |
| 5 | 503 | `BUSY` (retry) |
| 6 | 409 | `NO_WORK` (nothing to claim) |
| 7 | — | `CHECK_FAILED` (`ait check` found problems) |

Most refusals that an owner may override say so in `message` (`--force`).

## 4. CLI with `--json`

Run any command with `--json`; stdout then holds exactly one JSON object
(success shape below, or the error shape above). Identify yourself with
`AIT_SESSION=<name>` or `--as <name>` for claims and writes. `ait guide`
explains the workflow; `ait help` lists every option.

| command | success object |
|---|---|
| `init` | `{"db", "schema"}` |
| `add`, `edit` | `{"issue"}` |
| `state` | `{"issue", "rollup": [ ... ]}` — `rollup` lists containers that moved automatically |
| `move` | `{"issue"}` |
| `delete` | `{"deleted", "title"}` |
| `link`, `unlink` | `{"changed", "kind", "src", "dst"}` |
| `show` | `{"issue", "claim", "blockers", "criteria", "links", "notes", "note_count", "latest_handoff", "ready", "rollup"}` |
| `list` | `{"count", "issues": [issue + claim]}` |
| `next` | `{"mode": "build"\|"frame", "issues": [issue]}` |
| `next --claim`, `claim` | `{"issue", "claim", "branch", "latest_handoff", "renewed", "taken_from"}` |
| `next --explain ID` | `{"id", "ready", "blockers": [...], "frame_approval"}` |
| `heartbeat` | `{"session", "claims": [claim]}` |
| `verify` | `{"ok", "claim"}` |
| `release` | `{"released": id}` |
| `done` | `{"issue", "rollup"}` |
| `ship` | `{"released": [id], "ref", "warnings": [string]}` |
| `note` | `{"note": {"id", "issue", "kind", "body", "session", "generation", "at", "at_ms"}}` |
| `handoff` | `{"handoff"}` (saving) or `{"id", "handoff"}` (reading; `handoff` is null when there is none) |
| `criteria` | `{"id", "criteria": [criterion]}` |
| `board` | `{"counts", "in_progress", "ready", "stale", "attention", "done_recent", "head", "generated_at", "approvals_in_use"}` |
| `activity` | `{"head", "events": [event]}` |
| `roadmap` | `{"releases", "milestones", "features"}`, each item with a `rollup` |
| `stats` | see `/api/stats` |
| `check` | `{"ok", "issues", "problems": [{"code", "issue", "message"}]}` (exit 7 when not ok) |
| `export` / `import` | see `README.md` (export format 2) |

**Blocker** (in `show`, `next --explain`): `{"code", "message", "ids"?}` with
codes such as `STATE`, `TYPE`, `NEEDS`, `REVIEW_WAITS`, `WIP_LIMIT`,
`CLAIMED`, `STALE_CLAIM` and the approval codes.

**Rollup** (containers): `total`, `open`, `in_progress`, `in_review`,
`done`, `released`, `dropped`, `percent`, `blocked: [id]`,
`decisions: [id]`, `next` (id or null).

**Event** (activity): `seq` (int, strictly rising), `at`, `at_ms`,
`session`, `issue`, `action`, `details` (object; content depends on
`action`, informational only).

## 5. HTTP API (`ait serve`)

`ait serve [--port N] [--read-only]` listens on `127.0.0.1` only (default
port from `port` in `default.cfg`). Every request must carry
`Host: 127.0.0.1:<port>` or `localhost:<port>`, otherwise 403 (this blocks
DNS rebinding). Responses are `application/json; charset=utf-8`, carry
`Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, a framing
policy (see below), and close the connection.

### Reading (GET)

| path | body |
|---|---|
| `/api/version` | `{"api": 1, "ait": "0.1.0", "schema": 5, "writable": true}` |
| `/api/head` | `{"head": N}` — latest event `seq`; poll this cheaply and refetch when it changes |
| `/api/issues` | `{"head", "issues": [board item], "wip": {"in_progress": N, "in_review": N, "agent.<tier>": N}}` (configured limits only; count live items yourself from `issues`). Also `"agent_tiers": {tier: model}` (the configured `agent.<tier>` keys) |
| `/api/issue/<ID>` | same object as `ait show --json` (50 notes) |
| `/api/next` | `{"build": [issue], "frame": [issue]}` — top 10 of `next` and `next --frame` |
| `/api/board` | same as `ait board --json` |
| `/api/roadmap` | same as `ait roadmap --json` |
| `/api/stats?since=7d` | flow stats (`since` optional: `30m`, `12h`, `7d` ...) |
| `/api/activity?after=SEQ&limit=N` | same as `ait activity --json` (`limit` at least 1, default 50; the newest `limit` events) |

**Board item** (`/api/issues`): `id`, `title`, `type`, `state`, `priority`,
`order`, `parent`, `agent`, `reason`, `done_at`, `released_at`, `column`
(see board columns), `session`, `generation`, `lease_until` (claim, or
null), `stale` (bool: claimed but the lease ran out), `wait` (string or
null: why it is in `waiting`, why a backlog review has not started, or why a card in `ready` is not offered to agents).

**Stats:** `{"since", "columns": {column: {"count", "avg_hours",
"median_hours", "p90_hours", "now", "now_avg_hours", "now_max_hours"}},
"cycle_time": {...}, "lead_time": {...}}`. `count`/`avg`/`median`/`p90`
describe finished stays in the column; `now*` describe issues sitting in it
now. Cycle time = first claim to done; lead time = added to done. Hours are
rounded to 0.1; `null` when there is no data.

### Writing (POST /api/run)

The dashboard writes by running the same CLI commands, so every gate of the
CLI applies (mandatory fields, criteria, reviews, WIP limits, claims).

```
POST /api/run
Content-Type: application/json
X-Ait-Token: <token>

{"args": ["edit", "CORE-1-00", "--priority", "P1"]}
```

- Allowed commands: `add`, `edit`, `state`, `move`, `done`, `note`, `link`,
  `unlink`, `ship`, `criteria`, `claim`, `release`. Others → 400 `USAGE`.
- Refused options: `--db`, `--config`, `--as`, `--help`, `--body-file`,
  `--text-file`, `--from-file`, `--out` → 400 `USAGE`.
- Writes are recorded as session `dashboard`. Changing an issue an agent
  holds needs `--force`, exactly as on the CLI.
- Success: `200 {"ok": true, "result": <the command's --json object>}`.
- Failure: the error object plus `"exit": <exit code>`, with the HTTP status
  from the table in section 3.
- `--read-only` servers answer 403 `READ_ONLY`.

**Guards** (each failure is 403 `FORBIDDEN` unless noted):

1. `X-Ait-Token` must equal the token of this server start (32 hex
   characters, new on every start).
2. `Content-Type` must be `application/json` (else 415).
3. If `Origin` is sent it must be the server itself
   (`http://127.0.0.1:<port>` / `http://localhost:<port>`) or listed in
   `allowed_origins`.
4. If `Sec-Fetch-Site` is sent it must be `same-origin`, unless the request
   comes from an `allowed_origins` origin.
5. Body at most 2 MB (else 400).

The built-in page reads the token from `<meta name="ait-token">`.

### Embedding in another local UI (`allowed_origins`)

A factory UI served from another local port can use the full API:

```
# default.cfg next to ait.exe
allowed_origins = http://localhost:5173
```

- Values are comma-separated origins on `localhost`, `127.0.0.1` or `[::1]`
  (http or https, optional port, no path). Anything else is a `CONFIG`
  error at start-up.
- Requests whose `Origin` is listed get CORS headers
  (`Access-Control-Allow-Origin: <that origin>`, `Vary: Origin`, methods
  `GET, POST`, headers `Content-Type, X-Ait-Token`).
- `OPTIONS` preflight: 204 for a listed origin, 403 for any other.
- `GET /api/token` → `{"token": "..."}` for a listed origin on a writable
  server; 403 otherwise. Fetch it once at start-up (and again after a 403,
  since the token changes when `ait serve` restarts).
- Every other origin is still refused, with or without the token.

Example from the factory UI page:

```js
const base = "http://127.0.0.1:7777";
const { token } = await (await fetch(`${base}/api/token`)).json();
const r = await fetch(`${base}/api/run`, {
  method: "POST",
  headers: { "Content-Type": "application/json", "X-Ait-Token": token },
  body: JSON.stringify({ args: ["state", "CORE-1-00", "planned"] }),
});
const out = await r.json();   // {ok, result} or {error, exit}
```

Alternatively, embed the whole dashboard with
`<iframe src="http://127.0.0.1:7777/">`. The page then talks to its own
server, so it needs no token handling, but framing is only allowed for
pages on an `allowed_origins` origin (`Content-Security-Policy:
frame-ancestors 'self' <origins>`); without the setting every response says
`X-Frame-Options: DENY`.

## 6. Changelog

- **1** (2026-11-26): first published contract. Columns backlog → released,
  `in_review` / `released` states, review issues, `ship`, needs with
  `--until`, WIP limits, acceptance criteria, container auto-rollup, flow
  stats, `allowed_origins`, `/api/version`, `/api/token`. `/api/issues` gained `agent_tiers`; claims gained `lease_minutes` and the
  dashboard may run `claim` and `release` (all additive).
