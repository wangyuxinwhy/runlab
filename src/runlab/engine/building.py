"""Fixing declarations into realizations as a standalone step.

Building is separated from execution because it is the slow, network-dependent,
failure-prone part: a Run that starts from an already-fixed realization fails
only for reasons that belong to the Agent.
"""

from dataclasses import dataclass

from runlab.container.engine import DockerEngine
from runlab.declarations.loading import BaseDeclaration, OverlayDeclaration
from runlab.engine.resolution import resolve_environment


@dataclass(frozen=True, slots=True)
class BuildResult:
    platform: str
    realization: str
    environment: str


async def build_environment(
    base: BaseDeclaration,
    /,
    *,
    overlays: list[OverlayDeclaration],
    rebuild: bool,
) -> BuildResult:
    engine = DockerEngine()
    resolved = await resolve_environment(
        engine, base=base, overlays=overlays, rebuild=rebuild
    )
    return BuildResult(
        platform=resolved.spec.base.platform or "",
        realization=resolved.image,
        environment=resolved.spec.digest,
    )
