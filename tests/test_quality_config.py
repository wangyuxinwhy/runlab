import tomllib
from pathlib import Path


def test_quality_tools_exclude_experiment_artifacts() -> None:
    root = Path(__file__).parents[1]
    config = tomllib.loads((root / "pyproject.toml").read_text())

    assert set(config["tool"]["ruff"]["extend-exclude"]) >= {"experiments", "runs"}
    commands = [step["cmd"] for step in config["tool"]["poe"]["tasks"]["check"]]
    assert "ruff format src tests" in commands
    assert "ruff check src tests" in commands
