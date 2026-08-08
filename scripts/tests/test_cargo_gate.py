import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
CARGO_GATE = REPO_ROOT / "scripts" / "cargo_gate.py"
RUN_SCRIPT = REPO_ROOT / "scripts" / "run.sh"

CARGO_GATE_SEMANTIC = 20
CARGO_GATE_INFRASTRUCTURE = 21
CARGO_GATE_UNKNOWN = 22


def write_executable(path: Path, body: str) -> None:
    path.write_text(body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def marker_lines(completed: subprocess.CompletedProcess[str]) -> list[str]:
    return [
        line
        for line in completed.stderr.splitlines()
        if line.startswith("FKST_LOCAL_ITERATION_RESULT:")
    ]


class LocalIterationMappingTests(unittest.TestCase):
    def run_with_verify_exit(self, status: int) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copy2(RUN_SCRIPT, scripts / "run.sh")
            write_executable(scripts / "verify.sh", f"#!/bin/sh\nexit {status}\n")
            return subprocess.run(
                [scripts / "run.sh", "test"],
                cwd=root,
                text=True,
                capture_output=True,
                check=False,
            )

    def test_build_semantic_evidence_maps_to_semantic(self) -> None:
        completed = self.run_with_verify_exit(12)

        self.assertEqual(completed.returncode, 12)
        self.assertEqual(
            marker_lines(completed),
            ["FKST_LOCAL_ITERATION_RESULT:v2:FAIL:SEMANTIC"],
        )

    def test_build_infrastructure_evidence_maps_to_infrastructure(self) -> None:
        completed = self.run_with_verify_exit(17)

        self.assertEqual(completed.returncode, 17)
        self.assertEqual(
            marker_lines(completed),
            ["FKST_LOCAL_ITERATION_RESULT:v2:FAIL:INFRASTRUCTURE"],
        )

    def test_build_without_evidence_remains_unknown(self) -> None:
        completed = self.run_with_verify_exit(19)

        self.assertEqual(completed.returncode, 19)
        self.assertEqual(
            marker_lines(completed),
            ["FKST_LOCAL_ITERATION_RESULT:v2:UNKNOWN:UNKNOWN"],
        )


class CargoGateEvidenceTests(unittest.TestCase):
    def run_gate(
        self,
        mode: str,
        cwd: Path,
        *,
        path: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        if path is not None:
            environment["PATH"] = path
        return subprocess.run(
            [sys.executable, CARGO_GATE, mode, "--workspace"],
            cwd=cwd,
            env=environment,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_compiler_diagnostic_with_source_span_is_semantic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text(
                '[package]\nname = "broken"\nversion = "0.1.0"\nedition = "2021"\n',
                encoding="utf-8",
            )
            (root / "src" / "lib.rs").write_text(
                'pub fn broken() -> u32 { "not a number" }\n',
                encoding="utf-8",
            )

            completed = self.run_gate("build", root)

        self.assertEqual(completed.returncode, CARGO_GATE_SEMANTIC, completed.stderr)

    def test_unavailable_cargo_execution_is_infrastructure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            write_executable(bin_dir / "cargo", "#!/missing/fkst-interpreter\n")
            completed = self.run_gate("build", REPO_ROOT, path=str(bin_dir))

        self.assertEqual(completed.returncode, CARGO_GATE_INFRASTRUCTURE)

    def test_nonzero_cargo_without_structured_evidence_is_unknown(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            write_executable(bin_dir / "cargo", "#!/bin/sh\nexit 101\n")
            path = os.pathsep.join([str(bin_dir), os.environ.get("PATH", "")])

            completed = self.run_gate("build", REPO_ROOT, path=path)

        self.assertEqual(completed.returncode, CARGO_GATE_UNKNOWN)

    def test_started_test_process_failure_is_semantic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text(
                '[package]\nname = "failing_test"\nversion = "0.1.0"\nedition = "2021"\n',
                encoding="utf-8",
            )
            (root / "src" / "lib.rs").write_text(
                "#[test]\nfn rejects_tree() { assert!(false); }\n",
                encoding="utf-8",
            )

            completed = self.run_gate("test", root)

        self.assertEqual(completed.returncode, CARGO_GATE_SEMANTIC, completed.stderr)


if __name__ == "__main__":
    unittest.main()
