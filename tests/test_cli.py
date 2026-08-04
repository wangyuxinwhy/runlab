"""The CLI is verified as an installed process, which is how agents call it."""

import json
import subprocess
import sys
from pathlib import Path


def run_cli(
    *arguments: str, cwd: Path | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "runlab", *arguments],
        capture_output=True,
        text=True,
        check=False,
        cwd=cwd,
    )


def test_docs_serve_the_bundled_reference_layer() -> None:
    listing = run_cli("docs", "list")

    assert listing.returncode == 0
    topics = json.loads(listing.stdout)["topics"]
    assert {"principles", "model", "architecture", "cli"} <= set(topics)

    content = run_cli("docs", "get", "principles")
    assert content.returncode == 0
    assert "Base + Overlay + Task -> Run" in json.loads(content.stdout)["content"]


def test_an_unknown_topic_fails_with_a_diagnostic() -> None:
    result = run_cli("docs", "get", "absent")

    assert result.returncode == 1
    assert "unknown documentation topic" in result.stderr
    assert result.stdout == ""


def test_schema_names_cover_every_public_model() -> None:
    result = run_cli("schema", "list")

    assert json.loads(result.stdout)["schemas"] == [
        "base",
        "overlay",
        "task",
        "lock",
        "run-record",
    ]


def test_base_check_reports_declaration_identity(tmp_path: Path) -> None:
    root = tmp_path / "pi"
    root.mkdir()
    (root / "Dockerfile").write_text("FROM scratch\n")
    (root / "base.json").write_text(
        json.dumps(
            {
                "output_protocol": "pi-session-jsonl",
                "logs": {"target": "/root/.pi/sessions"},
            }
        )
    )

    result = run_cli("base", "check", str(root))

    assert result.returncode == 0
    payload = json.loads(result.stdout)
    assert payload["name"] == "pi"
    assert payload["declaration"].startswith("sha256:")
    assert payload["output_protocol"] == "pi-session-jsonl"
    assert payload["locked"] is False


def test_task_check_reports_the_instruction_digest(tmp_path: Path) -> None:
    root = tmp_path / "analyze"
    root.mkdir()
    (root / "task.md").write_text("Write /artifacts/answer.md.\n")

    result = run_cli("task", "check", str(root))

    payload = json.loads(result.stdout)
    assert payload["instruction"].startswith("sha256:")
    assert payload["workspace"] is False


def test_an_invalid_declaration_fails_without_stdout(tmp_path: Path) -> None:
    root = tmp_path / "broken"
    root.mkdir()

    result = run_cli("task", "check", str(root))

    assert result.returncode == 1
    assert result.stdout == ""
    assert "required file is missing" in result.stderr


def test_stdout_stays_one_compact_json_object(tmp_path: Path) -> None:
    root = tmp_path / "empty-overlay"
    root.mkdir()
    (root / "overlay.json").write_text("{}")

    result = run_cli("overlay", "check", str(root))

    assert result.stdout.count("\n") == 1
    assert ", " not in result.stdout
    assert json.loads(result.stdout)["empty"] is True
