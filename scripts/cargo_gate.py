#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from typing import Any


SEMANTIC = 20
INFRASTRUCTURE = 21
UNKNOWN = 22


def record_evidence(path: Path, evidence: dict[str, Any]) -> None:
    encoded = (json.dumps(evidence, separators=(",", ":")) + "\n").encode()
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(descriptor, encoded)
    finally:
        os.close(descriptor)


def run_test_process(evidence_path: Path, command: list[str]) -> int:
    try:
        completed = subprocess.run(command, check=False)
    except OSError as error:
        record_evidence(
            evidence_path,
            {"kind": "test-spawn-error", "errno": error.errno},
        )
        print(f"cargo_gate.py: test executable could not start: {error}", file=sys.stderr)
        return 126

    if completed.returncode < 0:
        record_evidence(
            evidence_path,
            {"kind": "test-signal", "signal": -completed.returncode},
        )
        return min(128 - completed.returncode, 255)

    record_evidence(
        evidence_path,
        {"kind": "test-exit", "returncode": completed.returncode},
    )
    return min(completed.returncode, 255)


def compiler_message_is_semantic(message: dict[str, Any]) -> bool:
    if message.get("reason") != "compiler-message":
        return False
    diagnostic = message.get("message")
    if not isinstance(diagnostic, dict) or diagnostic.get("level") != "error":
        return False
    spans = diagnostic.get("spans")
    return isinstance(spans, list) and any(
        isinstance(span, dict)
        and isinstance(span.get("file_name"), str)
        and isinstance(span.get("line_start"), int)
        for span in spans
    )


def read_test_evidence(path: Path) -> list[dict[str, Any]]:
    evidence = []
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return evidence
    for line in lines:
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            evidence.append(value)
    return evidence


def classify_failure(
    compiler_rejected: bool,
    test_evidence: list[dict[str, Any]],
) -> int:
    semantic = compiler_rejected or any(
        item.get("kind") == "test-exit" and item.get("returncode") != 0
        for item in test_evidence
    )
    infrastructure = any(
        item.get("kind") == "test-spawn-error" for item in test_evidence
    )
    if semantic == infrastructure:
        return UNKNOWN
    return SEMANTIC if semantic else INFRASTRUCTURE


def cargo_runner_config(evidence_path: Path) -> str:
    runner = [
        sys.executable,
        str(Path(__file__).resolve()),
        "test-runner",
        str(evidence_path),
    ]
    return f"target.'cfg(all())'.runner={json.dumps(runner)}"


def run_cargo_gate(mode: str, cargo_args: list[str]) -> int:
    with tempfile.TemporaryDirectory(prefix="fkst-cargo-gate-") as directory:
        evidence_path = Path(directory) / "test-evidence.jsonl"
        command = ["cargo", mode, "--message-format=json"]
        if mode == "test":
            command.extend(["--config", cargo_runner_config(evidence_path)])
        command.extend(cargo_args)

        try:
            process = subprocess.Popen(
                command,
                stdout=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
        except OSError as error:
            print(f"cargo_gate.py: cargo could not start: {error}", file=sys.stderr)
            return INFRASTRUCTURE

        compiler_rejected = False
        assert process.stdout is not None
        for line in process.stdout:
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                sys.stdout.write(line)
                sys.stdout.flush()
                continue
            if not isinstance(message, dict):
                continue
            compiler_rejected = compiler_rejected or compiler_message_is_semantic(message)
            diagnostic = message.get("message")
            if message.get("reason") == "compiler-message" and isinstance(diagnostic, dict):
                rendered = diagnostic.get("rendered")
                if isinstance(rendered, str):
                    sys.stderr.write(rendered)
                    sys.stderr.flush()

        returncode = process.wait()
        if returncode == 0:
            return 0
        return classify_failure(compiler_rejected, read_test_evidence(evidence_path))


def main(arguments: list[str]) -> int:
    if len(arguments) >= 3 and arguments[1] == "test-runner":
        return run_test_process(Path(arguments[2]), arguments[3:])
    if len(arguments) < 2 or arguments[1] not in {"build", "test"}:
        print("usage: cargo_gate.py {build|test} [cargo arguments...]", file=sys.stderr)
        return 2
    return run_cargo_gate(arguments[1], arguments[2:])


if __name__ == "__main__":
    sys.exit(main(sys.argv))
