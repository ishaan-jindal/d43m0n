"""TabPFN sidecar: newline-delimited JSON over a unix socket.

Messages:
  {"type":"refit","rows":[[...]],"labels":[0,1]} -> {"type":"ok","rows":N,"degraded":bool}
  {"type":"predict","features":[...]}            -> {"type":"prediction","p_fail":0.37}
  {"type":"predict_batch","rows":[[...]]}        -> {"type":"predictions","p_fails":[...]}
  {"type":"health"}                              -> {"type":"ok","degraded":bool,...}

``tabpfn`` is imported lazily: a missing or broken install makes the sidecar
report ``degraded: true`` instead of crashing. The Rust daemon then uses its
baseline predictor.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import socketserver
import sys
import threading
from typing import Any

# CPU cap: at most 5000 training rows per refit (TabPFN practical CPU limit).
MAX_ROWS = 5000


class Model:
    """Owns the TabPFN classifier and its lazy import."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._clf: Any = None
        self._rows = 0
        self._width = 0
        self._error: str | None = None
        self._version: str | None = None

    @property
    def error(self) -> str | None:
        return self._error

    @property
    def degraded(self) -> bool:
        return self._clf is None

    def _load_tabpfn(self) -> Any:
        # Never import tabpfn at module import time: the server must start and
        # answer health even when the package is absent or its weights missing.
        if self._version is not None:
            try:
                import tabpfn  # noqa: PLC0415

                return tabpfn
            except Exception as exc:  # noqa: BLE001
                self._error = f"tabpfn import failed: {exc}"
                return None
        try:
            import tabpfn  # noqa: PLC0415
        except Exception as exc:  # noqa: BLE001
            self._error = f"tabpfn import failed: {exc}"
            return None
        self._version = getattr(tabpfn, "__version__", "unknown")
        return tabpfn

    def health(self) -> dict[str, Any]:
        with self._lock:
            # Probe the lazy import so `doctor`/`eval` can report importability
            # even before the first refit.
            if self._version is None:
                self._load_tabpfn()
            return {
                "type": "ok",
                "model": "tabpfn",
                "version": self._version,
                "degraded": self.degraded,
                "rows": self._rows,
                "error": self._error,
            }

    def refit(self, rows: list[list[float]], labels: list[int]) -> int:
        with self._lock:
            if not rows:
                return 0
            if len(rows) != len(labels):
                self._error = "rows and labels length mismatch"
                return 0
            # Most recent rows only (input is already in ts order).
            rows = rows[-MAX_ROWS:]
            labels = labels[-MAX_ROWS:]
            width = len(rows[0])
            if any(len(row) != width for row in rows):
                self._error = "ragged feature rows"
                return 0
            if len(set(labels)) < 2:
                self._clf = None
                self._error = "need both classes to fit TabPFN"
                return 0
            tabpfn = self._load_tabpfn()
            if tabpfn is None:
                self._clf = None
                return 0
            try:
                import numpy as np  # noqa: PLC0415

                clf = tabpfn.TabPFNClassifier(device="cpu")
                clf.fit(np.asarray(rows, dtype=float), np.asarray(labels, dtype=int))
            except Exception as exc:  # noqa: BLE001
                self._clf = None
                self._error = f"TabPFN fit failed: {exc}"
                return 0
            self._clf = clf
            self._rows = len(rows)
            self._width = width
            self._error = None
            return self._rows

    def predict(self, rows: list[list[float]]) -> list[float]:
        with self._lock:
            if self._clf is None:
                raise RuntimeError(self._error or "model not fitted")
            if not rows:
                return []
            if any(len(row) != self._width for row in rows):
                raise ValueError("feature width mismatch")
            import numpy as np  # noqa: PLC0415

            proba = self._clf.predict_proba(np.asarray(rows, dtype=float))
            classes = list(getattr(self._clf, "classes_", [0, 1]))
            if proba.ndim == 1:
                proba = proba.reshape(-1, 1)
            column = classes.index(1) if 1 in classes else 0
            return [float(row[column]) for row in proba]


MODEL = Model()


def dispatch(req: dict[str, Any]) -> dict[str, Any]:
    kind = req.get("type")
    try:
        if kind == "health":
            return MODEL.health()
        if kind == "refit":
            rows = req.get("rows") or []
            labels = req.get("labels") or []
            fitted = MODEL.refit(rows, labels)
            return {
                "type": "ok",
                "rows": fitted,
                "degraded": MODEL.degraded,
                "error": MODEL.error,
            }
        if kind == "predict":
            features = req.get("features") or []
            if MODEL.degraded:
                return {
                    "type": "prediction",
                    "p_fail": None,
                    "degraded": True,
                    "error": MODEL.error,
                }
            p_fail = MODEL.predict([features])[0]
            return {"type": "prediction", "p_fail": p_fail, "degraded": False}
        if kind == "predict_batch":
            rows = req.get("rows") or []
            if MODEL.degraded:
                return {
                    "type": "predictions",
                    "p_fails": None,
                    "degraded": True,
                    "error": MODEL.error,
                }
            return {
                "type": "predictions",
                "p_fails": MODEL.predict(rows),
                "degraded": False,
            }
    except Exception as exc:  # noqa: BLE001 - one bad request must not kill the server
        return {"type": "error", "code": "internal", "message": str(exc)}
    return {"type": "error", "code": "type"}


class Handler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        for raw in self.rfile:
            raw = raw.strip()
            if not raw:
                continue
            try:
                req = json.loads(raw)
                if not isinstance(req, dict):
                    raise ValueError("request must be an object")
            except (json.JSONDecodeError, ValueError):
                reply: dict[str, Any] = {"type": "error", "code": "bad_request"}
            else:
                reply = dispatch(req)
            self.wfile.write(json.dumps(reply).encode() + b"\n")
            self.wfile.flush()


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


def default_socket() -> str:
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if runtime:
        return os.path.join(runtime, "d43m0n", "sidecar.sock")
    return f"/tmp/d43m0n-{os.getuid()}-sidecar.sock"


def main() -> None:
    parser = argparse.ArgumentParser(prog="d43m0n_sidecar")
    parser.add_argument("--socket", default=default_socket())
    args = parser.parse_args()

    path = args.socket
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, mode=0o700, exist_ok=True)
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass

    server = Server(path, Handler)
    os.chmod(path, 0o600)

    def stop(_signum: int, _frame: Any) -> None:
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    try:
        server.serve_forever()
    finally:
        server.server_close()
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    sys.exit(main())
