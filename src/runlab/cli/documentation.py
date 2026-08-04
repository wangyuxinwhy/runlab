"""Serving the documentation layers that ship inside the installed package.

Two of the four documentation kinds travel with the code. Reference states the
mechanisms an Agent must not guess at, and explanation states the constraints
behind them; both stay version-matched to the binary being called. Tutorials
and how-to guides are deliberately absent: one-time setup and human workflows
cost an Agent context without changing what it can do.
"""

from importlib import resources

from runlab.core.errors import RunLabError

_LAYERS = ("reference", "explanation")
_ROOT = "runlab.docs"
_SUFFIX = ".md"


def list_topics() -> list[str]:
    """Return topics as `<layer>/<name>`, which is also how they are requested."""
    return sorted(
        f"{layer}/{entry.name.removesuffix(_SUFFIX)}"
        for layer in _LAYERS
        for entry in resources.files(f"{_ROOT}.{layer}").iterdir()
        if entry.name.endswith(_SUFFIX)
    )


def read_topic(topic: str, /) -> str:
    available = list_topics()
    if topic not in available:
        message = (
            f"unknown documentation topic: {topic}; available: {', '.join(available)}"
        )
        raise RunLabError(message)
    layer, _, name = topic.partition("/")
    return resources.files(f"{_ROOT}.{layer}").joinpath(f"{name}{_SUFFIX}").read_text()
