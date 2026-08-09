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
VERIFY_SCRIPT = REPO_ROOT / "scripts" / "verify.sh"

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


def write_fake_cargo(bin_dir: Path, body: str) -> str:
    bin_dir.mkdir()
    write_executable(bin_dir / "cargo", f"#!{sys.executable}\n{body}")
    return str(bin_dir)


def write_test_cargo(
    bin_dir: Path,
    root: Path,
    test_exit: int,
    *,
    harness_ready: bool = True,
) -> str:
    test_program = root / "test-program"
    list_exit = 0 if harness_ready else test_exit
    write_executable(
        test_program,
        "#!/bin/sh\n"
        f"[ \"${{1:-}}\" = \"--list\" ] && exit {list_exit}\n"
        f"exit {test_exit}\n",
    )
    return write_fake_cargo(
        bin_dir,
        "import json\n"
        "import subprocess\n"
        "import sys\n"
        "prefix = \"target.'cfg(all())'.runner=\"\n"
        "config = next(arg for arg in sys.argv if arg.startswith(prefix))\n"
        "runner = json.loads(config[len(prefix):])\n"
        f"completed = subprocess.run([*runner, {str(test_program)!r}], check=False)\n"
        "raise SystemExit(completed.returncode)\n",
    )


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

    def test_success_emits_pass(self) -> None:
        completed = self.run_with_verify_exit(0)

        self.assertEqual(completed.returncode, 0)
        self.assertEqual(
            marker_lines(completed),
            ["FKST_LOCAL_ITERATION_RESULT:v2:PASS:NONE"],
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

    def test_untyped_script_test_failure_remains_unknown(self) -> None:
        completed = self.run_with_verify_exit(21)

        self.assertEqual(completed.returncode, 21)
        self.assertEqual(
            marker_lines(completed),
            ["FKST_LOCAL_ITERATION_RESULT:v2:UNKNOWN:UNKNOWN"],
        )

    def test_unavailable_cargo_maps_to_infrastructure_end_to_end(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / "scripts"
            tests = scripts / "tests"
            supervisor_src = root / "crates" / "fkst-supervisor" / "src"
            tests.mkdir(parents=True)
            supervisor_src.mkdir(parents=True)
            (supervisor_src / "main.rs").write_text(
                "fn main() {}\n", encoding="utf-8"
            )
            (tests / "test_fixture.py").write_text(
                "import unittest\n\n"
                "class FixtureTest(unittest.TestCase):\n"
                "    def test_fixture(self):\n"
                "        self.assertTrue(True)\n",
                encoding="utf-8",
            )
            shutil.copy2(RUN_SCRIPT, scripts / "run.sh")
            shutil.copy2(VERIFY_SCRIPT, scripts / "verify.sh")
            shutil.copy2(CARGO_GATE, scripts / "cargo_gate.py")

            bin_dir = root / "bin"
            bin_dir.mkdir()
            write_executable(bin_dir / "cargo", "#!/missing/fkst-interpreter\n")
            required_commands = {
                "bash": shutil.which("bash"),
                "cat": shutil.which("cat"),
                "dirname": shutil.which("dirname"),
                "find": shutil.which("find"),
                "python3": sys.executable,
                "wc": shutil.which("wc"),
            }
            for name, command in required_commands.items():
                self.assertIsNotNone(command)
                (bin_dir / name).symlink_to(command)
            environment = os.environ.copy()
            environment["PATH"] = str(bin_dir)
            completed = subprocess.run(
                [scripts / "run.sh", "test"],
                cwd=root,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertEqual(completed.returncode, 17, completed.stderr)
        self.assertEqual(
            marker_lines(completed),
            ["FKST_LOCAL_ITERATION_RESULT:v2:FAIL:INFRASTRUCTURE"],
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
            path = write_fake_cargo(
                root / "bin",
                "import json\n"
                "message = {\n"
                "    'reason': 'compiler-message',\n"
                "    'message': {\n"
                "        'level': 'error',\n"
                "        'spans': [{'file_name': 'src/lib.rs', 'line_start': 1}],\n"
                "        'rendered': 'error: rejected\\n',\n"
                "    },\n"
                "}\n"
                "print(json.dumps(message))\n"
                "raise SystemExit(101)\n",
            )

            completed = self.run_gate("build", REPO_ROOT, path=path)

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

            completed = self.run_gate("build", REPO_ROOT, path=str(bin_dir))

        self.assertEqual(completed.returncode, CARGO_GATE_UNKNOWN)

    def test_harness_rejection_is_semantic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = write_test_cargo(root / "bin", root, 101)

            completed = self.run_gate("test", REPO_ROOT, path=path)

        self.assertEqual(completed.returncode, CARGO_GATE_SEMANTIC, completed.stderr)

    def test_non_harness_test_exit_is_unknown(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = write_test_cargo(root / "bin", root, 2)

            completed = self.run_gate("test", REPO_ROOT, path=path)

        self.assertEqual(completed.returncode, CARGO_GATE_UNKNOWN, completed.stderr)

    def test_test_exit_without_successful_harness_probe_is_unknown(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = write_test_cargo(
                root / "bin", root, 101, harness_ready=False
            )

            completed = self.run_gate("test", REPO_ROOT, path=path)

        self.assertEqual(completed.returncode, CARGO_GATE_UNKNOWN, completed.stderr)


if __name__ == "__main__":
    unittest.main()
