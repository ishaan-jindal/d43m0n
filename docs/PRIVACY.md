# Privacy

Poltergeist is local-only: nothing leaves the machine unless you opt into Sentry.

- All processing, storage, and inference happen on localhost.
- The only network traffic is loopback to Ollama (default `http://127.0.0.1:11434`).
- Shell hooks send command text over a local unix socket (mode 0600); it is never written to disk as-is.
- The daemon redacts before storage: paths → `<path>`, URLs → `<url>`, other arguments → `<arg>`; flags are kept.
- Commands matching secret patterns (AWS keys, GitHub/Slack/OpenAI tokens, `password=`/`token=`/`api_key=`, `Authorization:` headers, private-key headers) are dropped entirely — no row, no counters.
- The event store is SQLite under `~/.local/share/poltergeist/`; `poltergeist purge --all` deletes it.
- Daemon logs contain timings and identifiers, never command text.
- Raw command text exists only in daemon memory for the duration of one request.

## Sentry (opt-in, off by default)

Setting `observability.sentry = true` sends **timings and counters only** off-machine (span durations, token counts, predictor mode). It never sends command text, paths, redacted commands, or whisper text; a scrubbing `before_send` hook enforces this. Leave it off and Poltergeist stays fully offline.
