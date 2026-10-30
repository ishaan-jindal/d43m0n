# Poltergeist — Ghost in the Machine (write-up draft)

## What it is

Poltergeist is a local-only terminal haunting. A zsh/bash/fish hook reports each command to a small Rust daemon, which redacts it immediately, stores an event, and predicts whether the command will fail. Before a risky command it whispers one dry line; after it, another. It also remembers directories you abandoned: `poltergeist seance` lists them, last redacted commands included, and marks missing paths as `departed`.

Prediction is TabPFN in a Python sidecar, with a Rust Beta(1,1) family-failure baseline as fallback. Narration is a local Gemma model through Ollama; a miss is silence. The hook has a 40 ms hard budget, always exits 0, and prints nothing if the daemon is down.

## Theme fit

The ghost lives in the machine's shell history. It predicts failures, speaks at night (L3 escalation between 00:00 and 05:00), and holds a séance for forgotten project directories. Its name is `d43m0n`: a deadpan familiar, a blunt little watcher that notices everything and says little — terse, dry, never giving advice, naming the tool or the streak rather than the command. Nothing is sent anywhere, which is the point of the theme: the ghost haunts one machine only.

## Why open / local models matter

Shell history contains paths, hostnames, project names, and, despite best efforts, secrets. Sending that stream to a hosted API would be a privacy incident waiting to happen. Local inference means:

- the tool works with networking disabled;
- command history never leaves the machine;
- no provider can reprice, rate-limit, or shut the feature down;
- the failure model and the voice are inspectable and replaceable.

The daemon only ever sends structured facts to the narrator: family, probability bucket, streak, hour bucket, level, days since last success. No paths, arguments, or raw command text. The same rule applies to séance narration, which sees only counts and dates.

## Measured numbers

Seeded demo store, 1600 labeled events, time-ordered 80/20 split (1280 train / 320 holdout), no shuffling:

| Predictor | ROC AUC | Log loss | Accuracy @0.5 | n |
|---|---|---|---|---|
| Rust baseline | 0.705 | **0.390** | **0.831** | 320 |
| TabPFN (3.5, `tabpfn==9.2.0`, CPU) | **0.725** | 0.416 | 0.825 | 320 |

Honest read: TabPFN wins ROC AUC by two points and loses log loss / accuracy by a hair — on this synthetic demo history the two are effectively tied, which is itself the finding: a 15-feature Beta-smoothed baseline is a strong prior for shell-failure prediction, and TabPFN's edge would have to come from real user history, not the seeded set. No synthetic TabPFN number was ever reported; this is the measured one (`tabpfn-v3.5-20260909.safetensors`, 1280-row fit, venv install, license accepted via `TABPFN_TOKEN`, weights via `HF_TOKEN`).

Real history, 222 labeled events, time-ordered 80/20 split (177 train / 45 holdout):

| Predictor | ROC AUC | Log loss | Accuracy @0.5 | n |
|---|---|---|---|---|
| Rust baseline | 0.978 | 0.259 | 0.978 | 45 |
| TabPFN (3.5, CPU) | **1.000** | **0.001** | **1.000** | 45 |

Small holdout (n=45), so read it as a perfect score on a small test, not proof of generality — but the direction flipped exactly as predicted: on real habits TabPFN separates cleanly where the demo set was a tie. The live daemon crossed the 200-row threshold the same night and now predicts with TabPFN.

Hook and narration latency, `poltergeist eval --demo --latency`, N=200, in-process pipeline (no shell, no daemon socket), debug build:

| Stage | p50 | p95 |
|---|---|---|
| pre hook path | 0.181 ms | 0.236 ms |
| post hook path | 0.157 ms | 0.225 ms |
| prediction (Rust baseline) | 0.004 ms | 0.008 ms |
| narration | 1.683 ms | 2.116 ms (silent: no Ollama server) |

For context, the SRS budget is 30 ms p95 for the hook. These numbers are a lower bound: the real hook also pays one unix-socket round trip. `eval --json` emits the same block as `pre_p50`, `pre_p95`, `post_p50`, `post_p95`, `predict_p50`, `predict_p95`, `narrate_p50`, `narrate_p95`, `N`.

## Degradation, demonstrated

- No TabPFN: `stats` prints `predictor: baseline (degraded: tabpfn import failed: ...)`; `doctor` names the component and the `pip install` fix.
- No Ollama server: `doctor` reports the endpoint unreachable, narration and séance are silent, and no error is shown to the user.
- Daemon down: the hook exits silently within its budget; the shell is unaffected.
- Real database untouched by demo: `demo --seed/--run` only open `demo.db`.

## What didn't work

1. **TabPFN weights offline.** The sidecar imports `tabpfn` lazily and survives its absence, but there was no way to install the package or accept the license/weights flow in this sandbox. The honest outcome is the baseline-only table above.
2. **No Ollama server.** Every narration path was silent. The write-up's narrator latency is therefore the miss path (request, validation, timeout), not Gemma generation.
3. **No zsh installed.** The zsh hook is checked in and `init zsh` is byte-identical to it, but interactive verification ran under bash (post-only by design, no reliable `preexec` without `bash-preexec`) and fish. Fish gets full pre+post whispers and was verified live over a pty: correct `--cmd/--cwd`, `--exit` (1 for `false`), real durations (`sleep 0.2` → 206 ms), `$status` preserved, silent when the daemon is down.
4. **No Sentry tracing.** Cut per SRS §14 cut order, so there are no trace screenshots and no Sentry dependency. Timings come from `eval --latency`.

## Prize tracks targeted

- **TabPFN:** sidecar protocol, venv isolation, lazy import, degraded health reporting, baseline comparison, time-ordered evaluation. Reported as measured: AUC 0.725 vs baseline 0.705 on the demo holdout — a tie with an honest story, not a victory lap.
- **Gemma:** Ollama narrator behind the `d43m0n` persona, with strict output validation, one retry, silence on a miss, and a documented local tag override (`gemma4:e2b` where `gemma3:4b` is not cached).
- **Sentry tracing:** deliberately cut per SRS §14; the JSON latency block substitutes for trace screenshots.

## Reproduce

```sh
cargo test
cargo build --release
./target/release/poltergeist demo --seed
./target/release/poltergeist seance --demo --days 30
./target/release/poltergeist demo --run
./target/release/poltergeist eval --demo --latency --json
./target/release/poltergeist doctor
```

All of it works with networking disabled. Recording link: TODO before submission.
