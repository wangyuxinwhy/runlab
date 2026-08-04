"""Serving the reference layer that ships inside the installed package.

The documents travel with the code so that guidance always matches the binary
being called, which is why the reference layer is self-contained and its
relative links never leave it.
"""

from importlib import resources

from runlab.core.errors import RunLabError

_PACKAGE = "runlab.docs.reference"
_SUFFIX = ".md"


def list_topics() -> list[str]:
    return sorted(
        entry.name.removesuffix(_SUFFIX)
        for entry in resources.files(_PACKAGE).iterdir()
        if entry.name.endswith(_SUFFIX)
    )


def read_topic(topic: str, /) -> str:
    available = list_topics()
    if topic not in available:
        message = (
            f"unknown documentation topic: {topic}; available: {', '.join(available)}"
        )
        raise RunLabError(message)
    return resources.files(_PACKAGE).joinpath(f"{topic}{_SUFFIX}").read_text()
