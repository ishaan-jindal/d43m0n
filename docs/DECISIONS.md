# Decisions and [VERIFY] outcomes

Living log for SRS section 0, rule 2: every **[VERIFY]** item, the verified value, or the fallback used when verification fails.

Status legend: `PENDING` (not verified yet), `CONFIRMED` (value checked against the installed package/docs), `FALLBACK` (unavailable or failing; fallback in use).

## [VERIFY] items

| Item | SRS ref | Question | Status | Value / decision | Owner |
|---|---|---|---|---|---|
| TabPFN version & API | 3, FR-22 | Exact `tabpfn` package version and classifier API to target (challenge page says 3.5) | CONFIRMED | `tabpfn==9.2.0`; ships model 3.5. Use `TabPFNClassifier.fit(X, y)` then `predict_proba(X)` | explorer |
| Ollama model tag | 3, 6.5, FR-31 | Exact locally available Gemma tag for `narrator.model` | CONFIRMED | `gemma3:4b` is a valid tag (3.4 GB) but the local cache currently has only `gemma4:e2b`. Keep default `gemma3:4b`; document a local override (`narrator.model = "gemma4:e2b"`) | explorer |
| TabPFN max training rows | FR-22, section 16 | Maximum supported training-set size; use `min(N, limit)` | CONFIRMED | CPU cap: `min(N, 5000)`; library hard limit 1,000,000 rows | explorer |
| Anomaly signal | FR-26, section 16 | Does TabPFN expose an anomaly/density score? Else `anomaly_score = 1 - frequency_rank_percentile` | FALLBACK | No anomaly/density API in core `tabpfn`; use `anomaly_score = 1 - frequency_rank_percentile` per SRS | explorer |

Rules for closing a row:

- Check the installed package or official docs; never invent an API.
- On failure, name the fallback used (predictor: Rust baseline; narrator: canned lines) and record it here.
- Keep this file current; it ships with the repo.

## Deviations from the SRS

- **Sentry (0.49.5):** `before_send` does not scrub transaction events. Enforce timings-only by allowlist at span/event creation (durations, token counts, predictor mode); never rely on `before_send` alone for command text, paths, redacted commands, or whisper text. SRS refs: 9.4, 11.1.
