#!/usr/bin/env python3
"""Encrypted byte custody for one publication attempt; never print raw output."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
from pathlib import Path


class CaptureError(RuntimeError):
    pass


class ReceiveCapture:
    def __init__(self, root: Path, *, age: Path, recipient: str, binding: dict):
        self.root, self.age, self.recipient = root, age.resolve(), recipient
        self.binding = binding
        self.records: list[dict] = []
        self.receipt_available = True
        try:
            root.mkdir(mode=0o700)
        except OSError:
            raise CaptureError("encrypted capture requires a new private directory") from None
        self.readiness = self._seal("readiness.age", b"")
        if self.readiness["status"] != "encrypted":
            raise CaptureError("encrypted capture readiness failed before publication")
        self._flush()
        if not self.receipt_available:
            raise CaptureError("encrypted capture receipt is unavailable before publication")

    def _seal(self, name: str, raw: bytes) -> dict:
        target = self.root / name
        try:
            descriptor = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "wb") as stream:
                result = subprocess.run(
                    [str(self.age), "-r", self.recipient], input=raw, stdout=stream,
                    stderr=subprocess.PIPE, timeout=120, env={"PATH": os.defpath},
                )
            if result.returncode != 0:
                raise CaptureError("encryption failed")
            data = target.read_bytes()
            if not data.startswith(b"age-encryption.org/v1\n"):
                raise CaptureError("encrypted output header is unavailable")
            return {"status": "encrypted", "file": name, "bytes": len(data),
                    "sha256": hashlib.sha256(data).hexdigest()}
        except (OSError, subprocess.SubprocessError, CaptureError):
            try:
                target.unlink(missing_ok=True)
            except OSError:
                pass
            # Neither encryption diagnostics nor exception text are public data.
            return {"status": "unavailable"}

    def snapshot(self) -> dict:
        complete = self.receipt_available and all(
            record[stream]["encrypted"]["status"] == "encrypted"
            for record in self.records for stream in ("stdout", "stderr")
        )
        return {"schema": "history-rewrite-receive-capture-v1", "binding": self.binding,
                "status": "complete" if complete else "incomplete", "records": self.records,
                "readiness": self.readiness, "receipt_available": self.receipt_available,
                "recipient_sha256": hashlib.sha256(self.recipient.encode()).hexdigest()}

    def _flush(self) -> None:
        try:
            target = self.root / "capture.json"
            temporary = self.root / "capture.json.pending"
            with temporary.open("w", encoding="utf-8") as stream:
                json.dump(self.snapshot(), stream, sort_keys=True, separators=(",", ":"))
                stream.write("\n")
            temporary.chmod(0o600)
            temporary.replace(target)
        except OSError:
            self.receipt_available = False

    def record(self, operation: str, exit_code: int, stdout: bytes, stderr: bytes) -> None:
        operation = operation if operation in {"push", "ls-remote", "init", "fetch", "cat-file", "rev-parse", "fsck"} else "git"
        sequence = len(self.records) + 1
        record = {"sequence": sequence, "operation": operation, "exit_code": exit_code}
        # Seal both untouched byte streams before the caller classifies or decodes them.
        for name, raw in (("stdout", stdout), ("stderr", stderr)):
            record[name] = {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest(),
                            "encrypted": self._seal(f"{sequence:04d}-{operation}-{name}.age", raw)}
        self.records.append(record)
        self._flush()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--age", type=Path, required=True)
    parser.add_argument("--recipient", required=True)
    args = parser.parse_args()
    try:
        ReceiveCapture(args.root, age=args.age, recipient=args.recipient, binding={"mode": "readiness"})
    except CaptureError as exc:
        parser.exit(1, f"{exc}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
