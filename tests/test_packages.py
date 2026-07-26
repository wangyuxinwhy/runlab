from pathlib import Path

import pytest

from runlab.errors import DefinitionError
from runlab.packages import load_environment, load_experiment, load_task


def test_environment_and_task_protocol(tmp_path: Path) -> None:
    environment = tmp_path / "environment"
    environment.mkdir()
    (environment / "Dockerfile").write_text("FROM scratch\n")
    task = tmp_path / "task"
    task.mkdir()
    (task / "task.md").write_text("Do the work.")

    assert load_environment(environment).identity.name == "environment"
    assert load_task(task).identity.name == "task"


def test_task_rejects_dockerfile(tmp_path: Path) -> None:
    task = tmp_path / "task"
    task.mkdir()
    (task / "task.md").write_text("Do the work.")
    (task / "Dockerfile").write_text("FROM scratch\n")

    with pytest.raises(DefinitionError, match="must not contain"):
        load_task(task)


def test_experiment_is_cartesian_product(tmp_path: Path) -> None:
    (tmp_path / "experiment.json").write_text('{"name":"matrix"}')
    environments = tmp_path / "environments"
    tasks = tmp_path / "tasks"
    environments.mkdir()
    tasks.mkdir()
    for name in ("a", "b"):
        root = environments / name
        root.mkdir()
        (root / "Dockerfile").write_text("FROM scratch\n")
    for name in ("one", "two", "three"):
        root = tasks / name
        root.mkdir()
        (root / "task.md").write_text(name)

    package = load_experiment(tmp_path)
    assert len(package.environments) * len(package.tasks) == 6


def test_environment_accepts_selected_build_input(tmp_path: Path) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n")
    (tmp_path / "environment.json").write_text(
        """
        {
          "build_inputs": [
            {
              "name": "tool_source",
              "source_env": "TOOL_SOURCE",
              "include": ["pyproject.toml", "src"]
            }
          ]
        }
        """
    )

    package = load_environment(tmp_path)
    assert package.definition.build_inputs[0].include == ("pyproject.toml", "src")


def test_codex_environment_requires_native_logs(tmp_path: Path) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n")
    (tmp_path / "environment.json").write_text('{"output_protocol":"codex-jsonl"}')

    with pytest.raises(DefinitionError, match="must declare native logs"):
        load_environment(tmp_path)


def test_environment_accepts_opaque_credential_slots(tmp_path: Path) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n")
    (tmp_path / "environment.json").write_text(
        """
        {
          "credentials": [
            {
              "name": "pi",
              "kind": "file",
              "target": "/run/credentials/pi/auth.json"
            },
            {
              "name": "lark",
              "kind": "directory",
              "target": "/run/credentials/lark-cli"
            }
          ]
        }
        """
    )

    package = load_environment(tmp_path)

    assert package.definition.credentials[0].name == "pi"
    assert package.definition.credentials[0].kind == "file"
    assert package.definition.credentials[0].target == "/run/credentials/pi/auth.json"
    assert package.definition.credentials[1].kind == "directory"
    assert package.definition.credentials[1].target == "/run/credentials/lark-cli"


def test_environment_rejects_duplicate_credential_slots(tmp_path: Path) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n")
    (tmp_path / "environment.json").write_text(
        """
        {
          "credentials": [
            {"name": "runtime", "kind": "file", "target": "/first"},
            {"name": "runtime", "kind": "file", "target": "/second"}
          ]
        }
        """
    )

    with pytest.raises(DefinitionError, match="credential names must be unique"):
        load_environment(tmp_path)
