#!/usr/bin/env python3
"""Generate the bundled synthetic demo history.

Output is a CSV mirroring the `events` table, minus the autoincrement `id`
column. Everything is stdlib and deterministic for a given --seed.

Patterns baked into the data:
  * cargo fails more often at night (22:00-04:59 UTC)
  * npm fails in one project directory
  * pytest is flaky in the backend directory
  * failures come in streaks; commands cluster into per-directory sessions
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import random
import sys
from datetime import datetime, timezone
from pathlib import Path

COLUMNS = [
    "ts",
    "cwd_id",
    "family",
    "cmd_redacted",
    "exit_code",
    "duration_ms",
    "hour",
    "dow",
    "prev_exit",
    "fail_streak",
    "secs_since_prev",
    "git_dirty",
    "arg_count",
    "has_pipe",
    "has_redirect",
    "p_fail_pred",
    "predictor",
]

# path -> (families plausible in that directory, weights)
DIRS = {
    "/home/dev/work/storefront": (("npm", "git", "python3", "curl"), 3),
    "/home/dev/work/storefront/packages/ui": (("npm", "git", "make"), 2),
    "/home/dev/work/api": (("cargo", "pytest", "git", "docker"), 3),
    "/home/dev/work/analytics": (("python3", "pytest", "git", "make"), 2),
    "/home/dev/work/infra": (("docker", "kubectl", "ssh", "systemctl"), 2),
    "/home/dev/work/notes": (("git", "grep"), 2),
    "/home/dev/scratch": (("make", "python3", "grep"), 1),
}

BASE_P_FAIL = {
    "git": 0.05,
    "cargo": 0.20,
    "npm": 0.22,
    "pytest": 0.30,
    "python3": 0.18,
    "make": 0.15,
    "docker": 0.12,
    "kubectl": 0.25,
    "ssh": 0.08,
    "grep": 0.04,
    "curl": 0.10,
    "systemctl": 0.10,
}

# Extra failure probability at night (22:00-04:59 UTC).
NIGHT_BOOST = {"cargo": 0.22, "pytest": 0.08, "make": 0.06, "docker": 0.05}

# Extra failure probability in a specific directory.
DIR_BOOST = {
    ("/home/dev/work/storefront", "npm"): 0.42,
    ("/home/dev/work/storefront/packages/ui", "npm"): 0.20,
    ("/home/dev/work/api", "pytest"): 0.15,
}

# family -> regex-shaped redacted templates (flags kept, everything else a placeholder)
TEMPLATES = {
    "cargo": (
        ("build", "--release"),
        ("test", "--workspace"),
        ("clippy",),
        ("run", "<arg>"),
    ),
    "git": (
        ("status",),
        ("commit", "-m", "<arg>"),
        ("push", "<arg>"),
        ("pull",),
        ("diff", "<path>"),
    ),
    "npm": (("run", "build"), ("run", "<arg>"), ("install", "<arg>"), ("test",)),
    "pytest": (("<path>",), ("-q", "<path>"), ("<arg>",)),
    "python3": (("<path>",), ("-m", "<arg>")),
    "make": (("test",), ("<arg>",)),
    "docker": (("compose", "up", "<arg>"), ("ps",), ("build", "<path>")),
    "kubectl": (("get", "<arg>"), ("apply", "-f", "<path>")),
    "ssh": (("<arg>",), ("-p", "<arg>", "<arg>")),
    "grep": (("-r", "<arg>", "<path>"),),
    "curl": (("-s", "<url>"), ("-X", "<arg>", "<url>")),
    "systemctl": (("status", "<arg>"),),
}

EXIT_CODES = {
    "git": (128, 1),
    "cargo": (101,),
    "npm": (1,),
    "pytest": (1,),
    "python3": (1,),
    "make": (2,),
    "docker": (125, 1),
    "kubectl": (1,),
    "ssh": (255,),
    "grep": (1,),
    "curl": (7, 28),
    "systemctl": (1,),
}

BASE_MS = {
    "git": 220,
    "cargo": 2500,
    "npm": 1800,
    "pytest": 3200,
    "python3": 600,
    "make": 1500,
    "docker": 900,
    "kubectl": 400,
    "ssh": 1200,
    "grep": 80,
    "curl": 700,
    "systemctl": 150,
}

DEFAULT_OUT = Path(__file__).resolve().parent.parent / "data" / "demo_history.csv"
DEFAULT_START = "2026-08-10"


def cwd_id(path: str) -> str:
    return hashlib.sha256(path.encode()).hexdigest()[:16]


def fail_probability(family: str, cwd: str, hour: int, streak: int) -> float:
    p = BASE_P_FAIL[family]
    if hour >= 22 or hour < 5:
        p += NIGHT_BOOST.get(family, 0.0)
    p += DIR_BOOST.get((cwd, family), 0.0)
    p += min(streak, 5) * 0.02
    return min(max(p, 0.01), 0.95)


def build_rows(seed: int, count: int, start: str) -> list[list[object]]:
    rng = random.Random(seed)
    try:
        ts = int(datetime.fromisoformat(start).replace(tzinfo=timezone.utc).timestamp())
    except ValueError as exc:
        raise SystemExit(f"invalid --start (YYYY-MM-DD expected): {exc}") from None

    paths = list(DIRS)
    weights = [DIRS[p][1] for p in paths]

    rows: list[list[object]] = []
    prev_ts: int | None = None
    prev_exit: int | None = None
    streak = 0
    commands_left = 0
    cwd = paths[0]

    while len(rows) < count:
        if commands_left <= 0:
            commands_left = rng.randint(4, 30)
            ts += rng.randint(3600, 20 * 3600)  # gap between sessions
            cwd = rng.choices(paths, weights=weights, k=1)[0]
        else:
            ts += rng.randint(5, 240)  # gap inside a session
        commands_left -= 1

        family = rng.choice(DIRS[cwd][0])
        hour = datetime.fromtimestamp(ts, timezone.utc).hour
        dow = datetime.fromtimestamp(ts, timezone.utc).weekday()  # Mon=0
        dow = (dow + 1) % 7  # SQLite convention: 0 = Sunday

        p = fail_probability(family, cwd, hour, streak)
        failed = rng.random() < p
        exit_code = rng.choice(EXIT_CODES[family]) if failed else 0

        duration = int(BASE_MS[family] * rng.lognormvariate(0, 0.7))
        if failed and family in ("cargo", "npm", "pytest", "make"):
            duration = int(duration * 1.6)
        duration = max(1, min(duration, 120_000))

        template = rng.choice(TEMPLATES[family])
        cmd_redacted = " ".join((family, *template))

        pred = round(min(max(p + rng.gauss(0, 0.07), 0.02), 0.98), 3)

        rows.append(
            [
                ts,
                cwd_id(cwd),
                family,
                cmd_redacted,
                exit_code,
                duration,
                hour,
                dow,
                "" if prev_exit is None else prev_exit,
                streak,
                "" if prev_ts is None else ts - prev_ts,
                1 if rng.random() < 0.45 else (0 if rng.random() < 0.85 else ""),
                len(template),
                1 if rng.random() < 0.02 else 0,
                1 if rng.random() < 0.03 else 0,
                pred,
                "baseline",
            ]
        )

        streak = streak + 1 if failed else 0
        prev_exit = exit_code
        prev_ts = ts

    return rows


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Generate the deterministic synthetic demo history (CSV, events-table columns).",
        epilog="Output never contains real commands: cmd_redacted is built from synthetic templates.",
    )
    parser.add_argument("--seed", type=int, default=42, help="RNG seed (default: 42)")
    parser.add_argument(
        "--rows",
        type=int,
        default=1600,
        help="number of events to generate; default 1600, minimum 1500",
    )
    parser.add_argument(
        "--start",
        default=DEFAULT_START,
        help=f"UTC start date, YYYY-MM-DD (default: {DEFAULT_START})",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=DEFAULT_OUT,
        help=f"output CSV path (default: {DEFAULT_OUT})",
    )
    args = parser.parse_args(argv)

    if args.rows < 1:
        parser.error("--rows must be positive")

    rows = build_rows(args.seed, args.rows, args.start)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("w", newline="", encoding="utf-8") as fh:
        writer = csv.writer(fh)
        writer.writerow(COLUMNS)
        writer.writerows(rows)

    print(f"wrote {len(rows)} events to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
