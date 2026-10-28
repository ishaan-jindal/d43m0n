# Poltergeist — Software Requirements Specification

Version 1.0 · Target: "Ghost in the Machine" open-source AI challenge (Halloween / Día de los Muertos week)

---

## 0. Instructions for the implementing agent

Read this whole document before writing code. Rules for working:

1. Build in the milestone order in section 14. Each milestone must run end to end before the next starts.
2. Do not invent library APIs. Where this spec names TabPFN, Ollama, or Sentry behavior, check the installed package or official docs first. Items marked **[VERIFY]** are known unknowns. If one fails verification, use the fallback given and note it in `docs/DECISIONS.md`.
3. The shell hook must never break, slow down, or block the user's shell. Every hook code path has a hard timeout and exits 0 on any error.
4. Raw command text and arguments must never be written to disk, logs, traces, or the narrator prompt. See section 9.
5. Keep the scope tight. Anything in section 13 (Out of scope) is not to be built.
6. Write tests alongside code (section 12). Commit in small steps with conventional commit messages.
7. Keep `README.md` current. It is a deliverable.

---

## 1. Purpose

Poltergeist is a local-only terminal "haunting". It watches shell activity, learns the user's habits, predicts which commands are likely to fail before they run, and comments on them in the voice of a ghost. It also recalls forgotten work from the user's own shell history.

Prediction uses TabPFN. Narration uses an open-weight Gemma model served locally. Nothing leaves the machine.

### 1.1 Why open models matter here (write-up thesis)

Shell history contains paths, hostnames, project names, and secrets. That is data users should not send to a hosted API. Local inference means the tool:

- works with the internet off;
- keeps command history on the user's machine;
- cannot be shut down or repriced by a provider.

The write-up must support this with measurements (section 11.3), not just claims.

---

## 2. Scope

### 2.1 In scope

- zsh hook (primary) and bash hook (best-effort, post-command only).
- Local daemon (Rust) with a SQLite event store.
- Failure-probability predictor (TabPFN sidecar in Python) with a Rust baseline fallback.
- Local narrator (Gemma via Ollama) with canned-line fallback.
- Haunting levels and time-of-day escalation.
- Features: Echoes, Séance, Demo mode, Eval command.
- Optional Sentry tracing, opt-in, timings only.

### 2.2 Definitions

| Term | Meaning |
|---|---|
| Event | One executed command with outcome metadata |
| Family | The normalized primary command (e.g. `cargo`, `git`, `docker`) after stripping `sudo`, env assignments, and wrappers |
| Whisper | A single line of ghost commentary |
| p_fail | Predicted probability that a command exits with a failure code |
| Level | Haunting intensity, 0 to 3 |
| Pre-whisper | A warning shown before the command runs |
| Post-whisper | A remark shown after the command finishes |

---

## 3. Technology stack

| Component | Choice | Notes |
|---|---|---|
| Daemon + CLI | Rust (stable) | `tokio`, `clap`, `serde`, `serde_json`, `rusqlite` (bundled), `regex`, `reqwest` (localhost only), `tracing`, `anyhow`, `toml` |
| Predictor sidecar | Python 3.11+ | `tabpfn` package, `numpy`, `pandas`. **[VERIFY]** TabPFN version and API; the target is the latest release (3.5 per the challenge page). |
| Narrator | Ollama | Default model is a small Gemma variant, configurable. **[VERIFY]** the exact model tag available locally. |
| Store | SQLite | Single file, WAL mode |
| IPC (hook ↔ daemon) | Unix domain socket, newline-delimited JSON | No TCP listeners |
| IPC (daemon ↔ sidecar) | Unix domain socket, newline-delimited JSON | Daemon spawns and supervises the sidecar |
| Observability (optional) | Sentry | Off by default |

TabPFN local use requires accepting its non-commercial license and obtaining weights per Prior Labs' instructions. Document the setup steps in the README.

---

## 4. System architecture

```
 shell (zsh/bash)
   │  preexec / precmd hook
   ▼
 poltergeist hook  (tiny Rust client, hard timeout, always exit 0)
   │  unix socket, JSON lines
   ▼
 poltergeistd  (Rust daemon)
   ├── Redactor ──► Event store (SQLite)
   ├── Feature builder
   ├── Predictor client ──► tabpfn sidecar (Python)  [fallback: Rust baseline]
   ├── Level engine + rate limiter
   ├── Narrator client ──► Ollama (localhost)         [fallback: canned lines]
   └── Whisper cache (pre-computed lines per (family, cwd))
```

### 4.1 Key design decisions

1. **Two whisper channels.** A pre-whisper uses only cached predictions because the hook has about 40 ms. A post-whisper reads from lines the daemon generated asynchronously after the previous event, or a canned line.
2. **One redaction point.** The daemon is the only component that sees raw command text. It redacts immediately and never persists the original.
3. **Graceful degradation.** Sidecar down means baseline predictor. Ollama down or slow means canned lines. Daemon down means the hook silently does nothing.
4. **No training loop.** TabPFN fits in-context. The sidecar refits on the most recent N rows.

---

## 5. Functional requirements

### 5.1 Shell integration

| ID | Requirement |
|---|---|
| FR-1 | `eval "$(poltergeist init zsh)"` installs hooks using `add-zsh-hook` for `preexec` and `precmd`. |
| FR-2 | The zsh `preexec` hook sends a `pre` message with the command text, cwd, and timestamp, then prints the returned pre-whisper if any. Total hook time budget is 40 ms; on timeout it prints nothing. |
| FR-3 | The zsh `precmd` hook sends a `post` message with exit code and duration, then prints the returned post-whisper if any. Budget is 40 ms. |
| FR-4 | `poltergeist init bash` installs a `PROMPT_COMMAND`-based hook producing post-whispers only. Pre-whispers in bash are optional and only if `bash-preexec` is detected. |
| FR-5 | Whispers print to stderr on a separate line above the prompt, dim/italic, prefixed by a configurable glyph (default `👻`). Colors respect `NO_COLOR`. |
| FR-6 | Commands beginning with a space, or matching the user ignore list, are never sent to the daemon. |
| FR-7 | If the daemon is unreachable, the hook exits silently and instantly. It never auto-starts the daemon synchronously. Starting happens via `poltergeist daemon start` or on first `init`. |
| FR-8 | The hook must not alter `$?`, command output, or command execution in any way. |

### 5.2 Event capture and storage

| ID | Requirement |
|---|---|
| FR-10 | For each completed command, store one event row (schema in section 7). |
| FR-11 | Normalize the command to a **family** by stripping leading `sudo`, `env VAR=...`, `time`, `nohup`, `command`, and `VAR=value` prefixes; take the first remaining token's basename. |
| FR-12 | Store a **redacted command form** (section 9.2), never the raw text. |
| FR-13 | Resolve the cwd to a stable `cwd_id` (hash of the absolute path). Store the path itself in a separate `dirs` table so it can be purged independently. |
| FR-14 | Record `git_dirty` (bool) if the cwd is in a git repo. Use a cheap check with a 20 ms timeout; on timeout store null. |
| FR-15 | A background job prunes events older than a configurable retention (default 365 days). |

### 5.3 Prediction

| ID | Requirement |
|---|---|
| FR-20 | At `pre` time, the daemon builds a feature vector (section 8.1) and looks up a cached `p_fail` for `(family, cwd_id, hour_bucket)`. |
| FR-21 | On a cache miss, the daemon requests a prediction from the sidecar asynchronously, stores the result in the cache, and does not wait for it. |
| FR-22 | The sidecar fits a TabPFN classifier on the most recent `N` labeled events (default 2000, configurable) and answers `predict` requests. It refits after every `K` new events (default 50) and on startup. **[VERIFY]** the maximum supported training-set size and use `min(N, limit)`. |
| FR-23 | If fewer than `MIN_ROWS` (default 200) labeled events exist, the daemon uses the baseline predictor only. |
| FR-24 | The **baseline predictor** is a Rust implementation: per-family failure rate with a Beta(1,1) prior smoothing. It also serves as the comparison baseline in `poltergeist eval`. |
| FR-25 | Label definition: `fail = exit_code not in {0, 130}`. Exit codes 130 (Ctrl-C) and 148 (suspended) are excluded from training. The set is configurable. |
| FR-26 | **Anomaly signal (optional).** If TabPFN's anomaly or density features are available and easy to use, expose `anomaly_score`. **[VERIFY]**. Otherwise define it as `1 - frequency_rank_percentile` for the family in the current cwd. Do not block any milestone on this. |

### 5.4 Narration

| ID | Requirement |
|---|---|
| FR-30 | The narrator receives only structured, non-sensitive facts: `family`, `p_fail` bucket, `fail_streak`, `hour_bucket`, `level`, `days_since_last_success_in_dir`, `kind` (`pre` or `post`), persona name. It never receives arguments, paths, or raw command text. |
| FR-31 | The narrator is called through the Ollama HTTP API on localhost with a timeout of 800 ms (configurable). |
| FR-32 | Output is validated: single line, at most 12 words, no newlines, no markdown, no emoji beyond the configured glyph. A violation triggers one retry at most, then a canned line. |
| FR-33 | A canned-line bank exists for each (level, kind) with at least 8 lines each. Selection is random without immediate repeats. |
| FR-34 | Narration for the next prompt is generated asynchronously right after each event is stored and held in a one-slot buffer. `post` reads the buffer. If it is empty, use a canned line. |
| FR-35 | The system prompt defines the persona: a terse, dry, eerie ghost haunting the user's terminal. It never gives advice, never repeats the command, never insults the user's identity, only teases the situation. |

### 5.5 Haunting levels and rate limiting

| ID | Requirement |
|---|---|
| FR-40 | Level is computed by a pure function `level(p_fail, fail_streak, hour, config) -> 0..3`. |
| FR-41 | Default thresholds, all configurable: **L0** `p_fail < 0.30` and `streak < 2`. **L1** `0.30 <= p_fail < 0.60`. **L2** `p_fail >= 0.60` or `streak >= 3`. **L3** local hour in `[00:00, 05:00)` and computed level >= 1, or `p_fail >= 0.85`. |
| FR-41a | L0 produces no whisper. |
| FR-42 | A rate limiter caps whispers: no more than 1 whisper per 3 commands at L1, 1 per 2 at L2, and every qualifying command at L3. An absolute cooldown of 5 seconds applies. |
| FR-43 | `poltergeist mute` / `poltergeist unmute` toggle all whispers without stopping logging. `poltergeist off` stops logging as well. |

### 5.6 Echoes

| ID | Requirement |
|---|---|
| FR-50 | At `pre` time, if the same `(family, cwd_id)` has at least one successful past event and a past event that **failed** most recently, the daemon may emit an Echo: "this worked before" using `days_since_last_success_in_dir`. |
| FR-51 | Echoes use the same narrator and canned fallback path, with `kind = echo`. They count against the rate limiter. |

### 5.7 Séance

| ID | Requirement |
|---|---|
| FR-60 | `poltergeist seance` lists directories whose most recent event is older than `--days` (default 90), sorted oldest first, showing last-active date, event count, and the last 5 redacted command forms. |
| FR-61 | `--narrate` produces a short ghost remark per directory (up to 3 directories). Input to the narrator is only counts and dates. |
| FR-62 | If the directory no longer exists on disk, mark it as "departed". |

### 5.8 Demo mode

| ID | Requirement |
|---|---|
| FR-70 | `poltergeist demo --seed` loads a bundled synthetic history (at least 1500 events) into a separate database file, with realistic failure patterns by family, cwd, and hour. |
| FR-71 | `poltergeist demo --run` replays a scripted session through the hook pipeline so whispers at every level appear deterministically (fixed random seed). |
| FR-72 | Demo mode never touches the user's real database. |

### 5.9 Evaluation

| ID | Requirement |
|---|---|
| FR-80 | `poltergeist eval` performs a **time-ordered** split (default 80/20) of labeled events, scores TabPFN and the baseline on the holdout, and prints ROC AUC, log loss, and accuracy at threshold 0.5. |
| FR-81 | `eval --json` emits machine-readable output for the write-up. |
| FR-82 | `poltergeist eval --latency` measures p50/p95 of the full `pre` and `post` hook round trip over N synthetic calls, and prediction and narration latency separately. |

### 5.10 Administration

| ID | Requirement |
|---|---|
| FR-90 | `poltergeist daemon start|stop|status` manages the daemon (foreground flag for debugging). |
| FR-91 | `poltergeist stats` prints event count, failure rate, top families, and predictor mode in use (TabPFN or baseline). |
| FR-92 | `poltergeist purge [--dirs] [--all]` deletes stored data. `--all` removes the database. |
| FR-93 | `poltergeist doctor` checks: daemon up, sidecar up, TabPFN importable, Ollama reachable, configured model present, socket permissions, hook installed. It reports fixes, not stack traces. |

---

## 6. Interfaces

### 6.1 CLI surface

```
poltergeist init <zsh|bash>        # prints shell snippet
poltergeist daemon <start|stop|status> [--foreground]
poltergeist hook <pre|post> ...    # internal, used by shell snippet
poltergeist seance [--days N] [--narrate]
poltergeist eval [--json] [--latency]
poltergeist demo <--seed|--run>
poltergeist stats
poltergeist mute | unmute | off | on
poltergeist purge [--dirs|--all]
poltergeist doctor
```

### 6.2 Hook ↔ daemon protocol (newline-delimited JSON, version 1)

Request, pre:
```json
{"v":1,"type":"pre","cmd":"cargo build --release","cwd":"/home/u/proj","ts":1761600000,"git_dirty":true}
```
Reply:
```json
{"v":1,"type":"pre_reply","whisper":"It failed here three times before.","level":2,"p_fail":0.71}
```
Request, post:
```json
{"v":1,"type":"post","exit":101,"duration_ms":4210,"ts":1761600004}
```
Reply:
```json
{"v":1,"type":"post_reply","whisper":"Again?","level":2}
```
`whisper` is `null` when nothing should be shown. Unknown fields are ignored. Unknown versions produce `{"type":"error","code":"version"}` and the hook prints nothing.

### 6.3 Daemon ↔ sidecar protocol

```json
{"type":"refit"}                                  → {"type":"ok","rows":1834}
{"type":"predict","features":{ ... }}             → {"type":"prediction","p_fail":0.37}
{"type":"health"}                                 → {"type":"ok","model":"tabpfn","version":"..."}
```
The sidecar must answer `predict` within 300 ms (configurable). On timeout, the daemon marks the sidecar degraded and uses the baseline for subsequent requests until `health` succeeds.

### 6.4 Socket and file locations

- Socket: `$XDG_RUNTIME_DIR/poltergeist/poltergeist.sock`, fallback `/tmp/poltergeist-$UID.sock`. Permissions `0600`.
- Config: `~/.config/poltergeist/config.toml`
- Data: `~/.local/share/poltergeist/` (`events.db`, `demo.db`, `canned.toml`)
- Logs: `~/.local/share/poltergeist/daemon.log` (rotated; contains no command text)

### 6.5 Configuration (`config.toml`)

```toml
[general]
glyph = "👻"
persona = "poltergeist"
retention_days = 365
ignore = ["^ls$", "^cd( |$)", "^clear$"]   # regexes on the normalized form

[levels]
l1 = 0.30
l2 = 0.60
l3_night_start = 0
l3_night_end = 5
l3_p = 0.85
streak_l2 = 3

[predictor]
mode = "auto"              # auto | tabpfn | baseline
train_window = 2000
min_rows = 200
refit_every = 50
fail_exclude_codes = [130, 148]
predict_timeout_ms = 300

[narrator]
backend = "ollama"
endpoint = "http://127.0.0.1:11434"
model = "gemma3:4b"        # [VERIFY] set to a locally available Gemma tag
timeout_ms = 800
max_words = 12

[observability]
sentry = false
```

---

## 7. Data model (SQLite)

```sql
CREATE TABLE dirs (
  cwd_id   TEXT PRIMARY KEY,      -- hash of absolute path
  path     TEXT NOT NULL,
  first_ts INTEGER NOT NULL,
  last_ts  INTEGER NOT NULL
);

CREATE TABLE events (
  id            INTEGER PRIMARY KEY,
  ts            INTEGER NOT NULL,
  cwd_id        TEXT NOT NULL REFERENCES dirs(cwd_id),
  family        TEXT NOT NULL,
  cmd_redacted  TEXT NOT NULL,
  exit_code     INTEGER NOT NULL,
  duration_ms   INTEGER NOT NULL,
  hour          INTEGER NOT NULL,    -- local 0..23
  dow           INTEGER NOT NULL,    -- 0..6
  prev_exit     INTEGER,
  fail_streak   INTEGER NOT NULL,
  secs_since_prev INTEGER,
  git_dirty     INTEGER,             -- 0/1/NULL
  arg_count     INTEGER NOT NULL,
  has_pipe      INTEGER NOT NULL,
  has_redirect  INTEGER NOT NULL,
  p_fail_pred   REAL,                -- prediction made at pre time, for later audit
  predictor     TEXT                 -- 'tabpfn' | 'baseline' | NULL
);
CREATE INDEX idx_events_ts ON events(ts);
CREATE INDEX idx_events_family_cwd ON events(family, cwd_id, ts);
```

Rules: WAL mode, `PRAGMA synchronous=NORMAL`. All writes happen in the daemon. Raw command text has no column anywhere.

---

## 8. Prediction specification

### 8.1 Features (all known at `pre` time)

| Feature | Type | Notes |
|---|---|---|
| `hour` | int 0–23 | Also provide `hour_sin`/`hour_cos` |
| `dow` | int 0–6 | |
| `family_id` | categorical → int | Stable mapping stored in DB; unseen families map to `-1` |
| `cwd_id_int` | categorical → int | Stable mapping; unseen maps to `-1` |
| `prev_exit_nonzero` | 0/1 | |
| `fail_streak` | int | Capped at 10 |
| `secs_since_prev` | int | Log-scaled, capped |
| `git_dirty` | 0/1/NaN | |
| `arg_count` | int | |
| `has_pipe`, `has_redirect` | 0/1 | |
| `family_fail_rate_prior` | float | Failure rate for the family over events strictly before this row's timestamp, to avoid label leakage |
| `family_cwd_fail_rate_prior` | float | Same, per (family, cwd), with smoothing |

### 8.2 Leakage rules

- Rate features must be computed using only events with `ts` earlier than the row being featurized.
- Evaluation splits are time-ordered; random shuffles are forbidden.

### 8.3 Evaluation protocol

Report, on a time-ordered holdout: ROC AUC, log loss, and accuracy at 0.5 for (a) TabPFN, (b) baseline. Report on both the seeded demo dataset and the author's real history if enough labeled rows exist. State the sample sizes. If TabPFN does not beat the baseline, report that honestly; the write-up covers what was learned.

---

## 9. Privacy and security requirements

### 9.1 Principles

- All processing is local. The only network traffic allowed is loopback to Ollama. A user-enabled Sentry integration is the one exception (section 9.4).
- Raw command text exists only in memory in the daemon, during a single request.

### 9.2 Redaction

Produce `cmd_redacted` as: `family` + flags (tokens starting with `-` and matching `^-{1,2}[A-Za-z][\w-]*$`, with `=value` parts stripped) + placeholders for other tokens.

- Tokens that look like paths (`/`, `~`, `./`, `../`, or containing a path separator) → `<path>`
- URLs → `<url>`
- Everything else → `<arg>`

Example: `curl -H "Authorization: Bearer abc" https://x.io/a` → `curl -H <arg> <url>`.

### 9.3 Secret guard

If the raw command matches any secret pattern, **drop the event entirely** (no row, no counters). Minimum patterns: `AKIA[0-9A-Z]{16}`, `ghp_[A-Za-z0-9]{20,}`, `gho_`, `sk-[A-Za-z0-9]{20,}`, `xox[abp]-`, `(?i)(password|passwd|token|secret|api[_-]?key)\s*[=:]`, `(?i)authorization:`, private-key headers. Patterns are configurable and extendable. Add unit tests with positive and negative samples.

### 9.4 Observability and privacy

Sentry is opt-in (`observability.sentry = true`). When enabled, send **timings and counters only** (span durations, token counts, predictor mode). Never send command text, paths, redacted forms, or whisper text. Add a scrubbing `before_send` hook and a test that asserts no sensitive field names appear. Document clearly in the README that enabling Sentry sends data off-machine.

### 9.5 Local security

- Socket permissions 0600; refuse to start if the socket dir is world-writable.
- The sidecar listens only on a unix socket, never TCP.
- No `eval` of model output. Model output is only ever printed, after validation.
- Escape control characters and ANSI sequences in model output before printing.

---

## 10. Non-functional requirements

| ID | Requirement |
|---|---|
| NFR-1 | Hook round-trip p95 ≤ 30 ms on a normal dev laptop when the daemon is up, and ≤ 5 ms when it is down. |
| NFR-2 | Hook never blocks beyond its timeout (40 ms) and always exits 0. |
| NFR-3 | Daemon idle memory ≤ 50 MB (excluding sidecar and Ollama). |
| NFR-4 | Fully functional offline after initial model/weights setup. |
| NFR-5 | A crash of the sidecar or Ollama must not crash the daemon; the daemon restarts the sidecar with backoff. |
| NFR-6 | Works on Linux and macOS. Windows is out of scope. |
| NFR-7 | Single static-ish Rust binary plus a pip-installable sidecar. Setup in under 10 minutes from the README. |
| NFR-8 | No telemetry by default. |

---

## 11. Observability and deliverables

### 11.1 Optional Sentry tracing

Spans: `hook.pre`, `hook.post`, `predict` (attrs: mode, cache_hit), `narrate` (attrs: model, latency, output_tokens, fallback_used), `refit` (rows, duration). Surface p50/p95 in the write-up via screenshots or exported traces.

### 11.2 Repository layout

```
poltergeist/
├── README.md
├── docs/
│   ├── SRS.md               # this document
│   ├── DECISIONS.md         # [VERIFY] outcomes and deviations
│   └── PRIVACY.md
├── crates/
│   ├── poltergeist/         # CLI + hook client
│   └── poltergeistd/        # daemon (or a single crate with two bins)
├── sidecar/
│   ├── pyproject.toml
│   └── poltergeist_sidecar/
├── shell/
│   ├── zsh.zsh
│   └── bash.bash
├── data/
│   ├── canned.toml
│   └── demo_history.csv
├── scripts/
│   └── gen_demo_history.py
└── tests/
```

### 11.3 Challenge deliverables

- Public repo with README and a short demo recording (terminal capture).
- Write-up covering: what it does, why open models matter here, measured numbers (AUC vs baseline, hook p50/p95, narrator latency, offline operation demonstrated with networking disabled), and what did not work.
- Optional: landing page on a `.tech` domain, links to agent session logs, and Sentry trace screenshots.

---

## 12. Testing requirements

**Unit**
- Redaction: paths, URLs, flags, quoted strings, pipes, env prefixes.
- Secret guard: positive and negative samples per pattern.
- Family normalization (`sudo`, `env X=1`, `time`, aliases).
- `level()` function across the threshold table including night escalation.
- Rate limiter behavior.
- Output validator (length, newlines, ANSI stripping).

**Integration**
- Daemon + fake sidecar + fake Ollama server: pre/post flows, cache hit and miss, degradation to baseline and canned lines on timeout.
- Real zsh in a subprocess: hook installs, does not alter `$?`, does not block when the daemon is down.
- Protocol version mismatch handling.

**Evaluation scripts**
- `eval` reproducible with a fixed seed on the demo dataset.
- Latency benchmark script writes JSON consumed by the write-up.

**Acceptance (manual)**
- Disable networking and run a full demo session. All features work.
- Kill the sidecar and Ollama mid-session. The shell remains responsive and whispers fall back.

---

## 13. Out of scope

- Auto-fixing, suggesting corrections, or modifying/blocking commands.
- Fish, PowerShell, and Windows support.
- Cloud model backends.
- Multi-user or cross-machine sync.
- A GUI or web dashboard.
- Training or fine-tuning models.
- Mobile clients.

---

## 14. Milestones

**M1 — Skeleton (Day 1)**
Repo layout, config loader, SQLite schema, daemon with socket, hook client, zsh snippet, event logging with redaction and secret guard, canned whispers, level function, rate limiter, `doctor`, `stats`, `demo --seed`.
*Exit:* a real zsh session logs events and shows canned whispers; the hook never blocks.

**M2 — Prediction and narration (Day 2)**
Feature builder, Rust baseline predictor, Python sidecar with TabPFN, cache, async predict flow, Ollama narrator with validation and fallback, Echoes, `eval`.
*Exit:* pre-whispers driven by TabPFN predictions; narration from Gemma; AUC numbers produced.

**M3 — Theme and polish (Day 3)**
Séance, `demo --run`, `eval --latency`, optional Sentry, bash hook, README, write-up draft, recording.
*Exit:* all acceptance tests in section 12 pass.

**Cut order if behind schedule:** Sentry → bash hook → Séance narration → anomaly signal → Echoes.

---

## 15. Acceptance criteria

1. In a fresh zsh with the hook installed, running commands produces events in the database with no raw command text anywhere on disk.
2. With a seeded history, running a known-failing pattern produces an L2 pre-whisper from a cached TabPFN prediction.
3. At local time between 00:00 and 05:00, qualifying whispers escalate to L3.
4. `poltergeist eval` prints AUC for TabPFN and baseline on a time-ordered holdout.
5. Stopping the daemon leaves the shell fully responsive; hook overhead is ≤ 5 ms.
6. With Ollama stopped, canned whispers appear and no errors are shown to the user.
7. `poltergeist seance` lists dormant directories with redacted recent commands.
8. With networking disabled, all of the above still works.
9. `doctor` correctly diagnoses each stopped component.

---

## 16. Known risks and open questions

| Risk / question | Mitigation |
|---|---|
| Failure labels need exit-code history that shell history files lack | Seeded demo dataset; hook collects real labels from install onward |
| TabPFN CPU inference latency unknown | Async prediction plus caching; baseline fallback; benchmark in M2 |
| TabPFN license/weights setup friction | Document in README; `doctor` checks it |
| TabPFN max training rows and anomaly API unverified | **[VERIFY]**; use `min(N, limit)`; anomaly signal is optional |
| Small Gemma output quality varies | Strict validator, one retry, canned fallback |
| bash `preexec` is unreliable without helpers | Post-only bash support in v1 |
| Prediction may not beat the baseline on real data | Report honestly; the demo dataset documents the method |
