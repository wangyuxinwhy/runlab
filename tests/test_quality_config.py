import re
import tomllib
from pathlib import Path

_LIST_ITEM = re.compile(r"^(?:[-+*] |\d+\. )")


def test_quality_tools_exclude_experiment_artifacts() -> None:
    root = Path(__file__).parents[1]
    config = tomllib.loads((root / "pyproject.toml").read_text())

    assert set(config["tool"]["ruff"]["extend-exclude"]) >= {"experiments", "runs"}
    commands = [step["cmd"] for step in config["tool"]["poe"]["tasks"]["check"]]
    assert "ruff format src tests" in commands
    assert "ruff check src tests" in commands


def test_markdown_prose_uses_soft_wrapping() -> None:
    root = Path(__file__).parents[1]
    violations = [
        f"{path.relative_to(root)}:{line}"
        for path in root.rglob("*.md")
        if not any(part.startswith(".") for part in path.relative_to(root).parts)
        for line in _hard_wrapped_lines(path)
    ]

    assert violations == []


def _hard_wrapped_lines(path: Path) -> list[int]:
    violations: list[int] = []
    previous_can_wrap = False
    in_fence = False
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith(("```", "~~~")):
            in_fence = not in_fence
            previous_can_wrap = False
            continue
        if in_fence or _is_structure(line):
            previous_can_wrap = _LIST_ITEM.match(stripped) is not None
            continue
        if previous_can_wrap:
            violations.append(number)
        previous_can_wrap = bool(stripped)
    return violations


def _is_structure(line: str) -> bool:
    stripped = line.lstrip()
    return (
        not stripped
        or line.startswith("    ")
        or stripped.startswith(("#", "|", ">", "<"))
        or stripped in {"---", "***", "___"}
        or _LIST_ITEM.match(stripped) is not None
    )
