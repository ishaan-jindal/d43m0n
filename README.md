# d43m0n 👻

A local-only terminal haunting. d43m0n watches your shell, learns your habits, predicts which commands are likely to fail before they run, and comments in the voice of a ghost. It also surfaces work you forgot was still waiting in old directories.

Nothing leaves the machine: prediction runs on TabPFN in a local sidecar (or the built-in Rust baseline), narration on a local Gemma model via Ollama (silence when it misses), and the event store is SQLite under `~/.local/share/d43m0n/`. The shell hook has a 40 ms budget and degrades silently when the daemon is down.

**Status: M3** — zsh and bash hooks, redacted event logging, secret guard, baseline + TabPFN prediction, Gemma narration, Echoes, Séance, demo replay, eval + latency benchmark, `doctor`, `stats`, `purge`.

## Meet d43m0n

The default persona is `d43m0n`, a deadpan familiar with opinions — fonder of the terminal than it lets on, blunt about the tool and the hour, never giving advice or naming a path. Rename it in `~/.config/d43m0n/config.toml`:

```toml
[general]
persona = "someone else"
```

Narration is pure Gemma; on a miss d43m0n says nothing.

## Quickstart

Prerequisites: Rust stable, Python 3.11+ (only for the TabPFN sidecar), zsh, bash or fish.

```sh
cargo build --release
./target/release/d43m0n daemon start
eval "$(./target/release/d43m0n init zsh)"   # or: init bash
./target/release/d43m0n demo --seed          # 1600 events in demo.db
./target/release/d43m0n demo --run           # live replay
```

Bash gets post-whispers only (no `preexec`). `init` prints the snippet without touching your rc file; add the `eval` line yourself.

```sh
eval "$(./target/release/d43m0n init bash)"
```

Fish gets full pre- and post-whispers via `fish_preexec`/`fish_postexec`:

```sh
d43m0n init fish | source
```

## Séance

`seance` lists directories whose most recent event is older than `--days` (default 90), oldest first, with the last five redacted command forms. Missing paths are marked `departed`. `--narrate` adds a ghost remark for up to three directories; the narrator sees only counts and dates.

Real output from the bundled demo store:

```text
$ d43m0n seance --demo --days 30
séance: 7 directories dormant for 30+ days (oldest first)

/home/dev/scratch  (departed)
  last active 2026-09-01 · 42 events
  last commands: make test, grep -r <arg> <path>, make test, make <arg>, python3 <path>

/home/dev/work/storefront/packages/ui  (departed)
  last active 2026-09-18 · 209 events
  last commands: npm test, make <arg>, make <arg>, npm install <arg>, make test

... 5 more directories (all departed on this machine)
```

## Demo replay

`demo --run` replays 18 scripted commands through the level engine with cached predictions from `demo.db`; whisper text is generated live. It never opens your real database and starts no daemon.

```text
$ d43m0n demo --run
demo replay: 18 commands · live narrator · /home/sacred/.local/share/d43m0n/demo.db
(read-only over the demo store; your real database is untouched)

  [13:00] git status             p=0.12  · L0 (silent)
  [13:00] npm test               p=0.50  · L1 (rate-limited)
  [13:00] npm test               p=0.50  👻 L1 Your fingers hesitate. I noticed.
  [14:00] npm run build          p=0.60  · L2 (rate-limited)
  [14:00] npm run build          p=0.60  👻 L2 The room is already bracing.
  [02:00] python3 -m pytest      p=0.62  👻 L3 The dark has been waiting for this.
  [01:00] npm run build          p=0.69  👻 L3 Every failure here has fed something.
  [01:00] cargo clippy           p=0.46  👻 L3 It is late. The house is awake.

levels reached: {0, 1, 2, 3} · whispers: 7
```

## Evaluation

Time-ordered 80/20 holdout, no shuffling. Baseline is the Rust Beta(1,1) family failure rate; TabPFN is fitted in the sidecar when its weights are importable.

```text
$ d43m0n eval --demo --latency
eval: 1600 labeled events, time-ordered 80/20 (1280 train / 320 holdout)
events: 1600  train: 1280  holdout: 320  split: time-ordered 80/20
baseline  auc 0.705  logloss 0.3902  acc 0.831  n 320
tabpfn    unavailable: tabpfn not available: tabpfn import failed: No module named 'tabpfn'
latency: N=200 in-process (no shell, no daemon socket)
  pre      p50 0.181 ms  p95 0.236 ms
  post     p50 0.157 ms  p95 0.225 ms
  predict  p50 0.004 ms  p95 0.008 ms  (Rust baseline)
  narrate  p50 1.683 ms  p95 2.116 ms  (silent: no Ollama server)
```

`eval --json` emits the same numbers as a machine-readable object, including `latency` keys `pre_p50`, `pre_p95`, `post_p50`, `post_p95`, `predict_p50`, `predict_p95`, `narrate_p50`, `narrate_p95`, `N`. The latency block is an in-process pipeline measurement (debug build on the author's laptop); narration was silent because no Ollama server was running.

## Offline

Every command above works with networking disabled. There is no network listener and no cloud backend; the only traffic d43m0n ever attempts is loopback to Ollama. With Ollama off, narration is silent. With TabPFN absent, prediction falls back to the Rust baseline. `d43m0n doctor` names whichever component is missing and the fix.

## Predictor and narrator setup

TabPFN is not bundled. Local use requires accepting Prior Labs' non-commercial license and obtaining the weights per their instructions, then `pip install tabpfn==9.2.0` — on Arch's externally-managed Python, install into the sidecar venv instead: `~/.local/share/d43m0n/venv/bin/pip install "tabpfn==9.2.0"`. The daemon spawns the sidecar with `$D43M0N_PYTHON` (set in the fish snippet config and the systemd unit). Without it the sidecar reports `degraded` and the daemon reports `baseline`; `stats` never claims TabPFN unless the sidecar is healthy and the probe import succeeds.

First fit downloads weights after a one-time license acceptance. Headless: accept the license at https://ux.priorlabs.ai (Licenses tab), copy the key from https://ux.priorlabs.ai/account, add `Environment=TABPFN_TOKEN=<key>` under `[Service]` via `systemctl --user edit d43m0n`, and restart.

Narration uses Ollama on localhost. The default model is `gemma3:4b`; if your local cache has a different tag, set it in `~/.config/d43m0n/config.toml`:

```toml
[narrator]
model = "gemma4:e2b"   # local override; any installed Gemma tag works
```

Narration output is validated: one line, ≤12 words, no newlines, no markdown, no emoji. Two failed attempts produce silence.

Sentry tracing was cut early; there is no Sentry dependency and no telemetry of any kind.

## Privacy

Raw command text never touches disk. Events store only the family, a redacted form (`<path>`, `<url>`, `<arg>`, flags kept), exit code and timing. Commands matching secret patterns are dropped entirely. The event store is SQLite under `~/.local/share/d43m0n/`; `d43m0n purge --all` deletes it. No telemetry of any kind.

## Challenge

Built for the [Ghost in the Machine open-source AI challenge](https://dev.to/challenges/hacktoberfest-week4-2026-10-26) (Halloween / Día de los Muertos week, 2026).
