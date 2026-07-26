import json
from pathlib import Path

from click.testing import CliRunner

from runlab.cli import cli


def test_help_exposes_noun_verb_control_surface() -> None:
    result = CliRunner().invoke(cli, ["--help"])

    assert result.exit_code == 0
    assert "environment" in result.output
    assert "task" in result.output
    assert "run" in result.output
    assert "experiment" in result.output
    assert "guidance" in result.output
    assert "schema" in result.output


def test_task_check_is_compact_json(tmp_path: Path) -> None:
    (tmp_path / "task.md").write_text("Do the work.")

    result = CliRunner().invoke(cli, ["task", "check", str(tmp_path)])

    assert result.exit_code == 0
    assert "\n" not in result.output.rstrip("\n")
    assert '"name":"' in result.output


def test_experiment_check_reports_matrix_size(tmp_path: Path) -> None:
    (tmp_path / "experiment.json").write_text('{"name":"matrix"}')
    environment = tmp_path / "environments" / "agent"
    environment.mkdir(parents=True)
    (environment / "Dockerfile").write_text("FROM scratch\n")
    task = tmp_path / "tasks" / "work"
    task.mkdir(parents=True)
    (task / "task.md").write_text("Do the work.")

    result = CliRunner().invoke(cli, ["experiment", "check", str(tmp_path)])

    assert result.exit_code == 0
    assert result.output == ('{"name":"matrix","environments":1,"tasks":1,"runs":1}\n')


def test_environment_check_describes_credential_slots(tmp_path: Path) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n")
    (tmp_path / "environment.json").write_text(
        """
        {
          "credentials": [
            {
              "name": "claude",
              "kind": "file",
              "target": "/run/credentials/claude/setup-token"
            }
          ]
        }
        """
    )

    result = CliRunner().invoke(cli, ["environment", "check", str(tmp_path)])

    assert result.exit_code == 0
    value = json.loads(result.output)
    assert value["output_protocol"] == "opaque"
    assert value["logs"] is None
    assert value["credentials"] == [
        {
            "name": "claude",
            "kind": "file",
            "target": "/run/credentials/claude/setup-token",
        }
    ]


def test_environment_check_describes_runtime_log_contract(tmp_path: Path) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n")
    (tmp_path / "environment.json").write_text(
        """
        {
          "output_protocol": "claude-stream-json",
          "logs": {
            "target": "/home/node/.claude/projects"
          }
        }
        """
    )

    result = CliRunner().invoke(cli, ["environment", "check", str(tmp_path)])

    assert result.exit_code == 0
    value = json.loads(result.output)
    assert value["output_protocol"] == "claude-stream-json"
    assert value["logs"] == {"target": "/home/node/.claude/projects"}


def test_run_and_experiment_help_expose_one_credentials_directory() -> None:
    runner = CliRunner()

    run_help = runner.invoke(cli, ["run", "start", "--help"])
    experiment_help = runner.invoke(cli, ["experiment", "run", "--help"])

    assert run_help.exit_code == 0
    assert experiment_help.exit_code == 0
    assert "--credentials DIRECTORY" in run_help.output
    assert "--credentials DIRECTORY" in experiment_help.output
    assert "RUNLAB_CREDENTIALS" in run_help.output
    assert "RUNLAB_CREDENTIALS" in experiment_help.output


def test_embedded_environment_guidance_is_machine_readable() -> None:
    runner = CliRunner()

    listed = runner.invoke(cli, ["guidance", "list"])
    shown = runner.invoke(cli, ["guidance", "show", "environment"])

    assert listed.exit_code == 0
    assert json.loads(listed.output)["guidance"] == [
        {
            "name": "environment",
            "summary": (
                "Author an Agent runtime Environment and declare its RunLab contracts."
            ),
        }
    ]
    assert shown.exit_code == 0
    value = json.loads(shown.output)
    assert value["name"] == "environment"
    assert value["content"].startswith("# Authoring a RunLab Environment\n")
