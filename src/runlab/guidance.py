from __future__ import annotations

from importlib.resources import files

from runlab.errors import DefinitionError

_GUIDANCE = {
    "environment": (
        "Author an Agent runtime Environment and declare its RunLab contracts.",
        "environment.md",
    ),
}


def list_guidance() -> list[dict[str, str]]:
    return [
        {"name": name, "summary": summary}
        for name, (summary, _filename) in _GUIDANCE.items()
    ]


def read_guidance(name: str, /) -> str:
    try:
        _summary, filename = _GUIDANCE[name]
    except KeyError as error:
        msg = f"unknown guidance: {name}"
        raise DefinitionError(msg) from error
    return files("runlab").joinpath("guidance", filename).read_text(encoding="utf-8")
