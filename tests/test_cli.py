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
