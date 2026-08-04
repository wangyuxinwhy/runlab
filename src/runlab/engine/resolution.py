"""Resolving declarations into the realization chain a Run will execute.

Resolution is where an environment stops being a function and becomes a
constant. A locked realization that cannot be retrieved fails here instead of
being rebuilt, because a silent rebuild replaces the baseline that every
earlier Run was compared against.
"""

import shutil
import tempfile
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

from runlab.container.engine import BuildRequest, DockerEngine
from runlab.core.digest import digest_directory, digest_values
from runlab.core.errors import DeclarationError, RealizationError
from runlab.core.models import (
    BaseBinding,
    CredentialRequest,
    EnvironmentSpec,
    InputRequest,
    NetworkMode,
    OverlayBinding,
    RealizationKind,
    ResolvedBuildInput,
)
from runlab.declarations.loading import BaseDeclaration, OverlayDeclaration
from runlab.engine.binding import HostMount, environment_build_path, identify
from runlab.realization.chain import environment_digest
from runlab.realization.locking import (
    locked_realization,
    read_lock,
    record_realization,
    write_lock,
)


@dataclass(frozen=True, slots=True)
class ResolvedEnvironment:
    image: str
    spec: EnvironmentSpec
    mounts: list[HostMount]
    build_inputs: list[ResolvedBuildInput]
    credential_requests: list[CredentialRequest]
    input_requests: list[InputRequest]


async def resolve_environment(
    engine: DockerEngine,
    /,
    *,
    base: BaseDeclaration,
    overlays: Sequence[OverlayDeclaration],
    rebuild: bool,
) -> ResolvedEnvironment:
    platform = await engine.platform()
    base_image, build_inputs = await resolve_base(
        engine, base=base, platform=platform, rebuild=rebuild
    )
    image = base_image
    bindings: list[OverlayBinding] = []
    mounts: list[HostMount] = []
    env: dict[str, str] = {}
    network: NetworkMode = "default"
    credential_requests = list(base.definition.credentials)
    for overlay in overlays:
        image, binding = await _apply_overlay(
            engine, overlay=overlay, image=image, rebuild=rebuild
        )
        bindings.append(binding)
        mounts.extend(_overlay_mounts(overlay))
        env.update(overlay.definition.env)
        if overlay.definition.network is not None:
            network = overlay.definition.network
        credential_requests.extend(overlay.definition.credentials)
    spec = EnvironmentSpec(
        digest=environment_digest(
            base_image,
            overlay_realizations=[item.realization for item in bindings],
            env=env,
            network=network,
        ),
        base=BaseBinding(
            name=base.identity.name,
            declaration=base.identity.digest,
            realization=base_image,
            platform=platform,
        ),
        overlays=bindings,
        env=env,
        network=network,
    )
    return ResolvedEnvironment(
        image=image,
        spec=spec,
        mounts=mounts,
        build_inputs=build_inputs,
        credential_requests=credential_requests,
        input_requests=list(base.definition.inputs),
    )


async def resolve_base(
    engine: DockerEngine,
    /,
    *,
    base: BaseDeclaration,
    platform: str,
    rebuild: bool,
) -> tuple[str, list[ResolvedBuildInput]]:
    """Return the Base realization for one platform, plus its build inputs.

    The platform is the lock key because a multi-platform tag resolves to a
    different image per architecture, which would otherwise let two Runs share
    a realization identity without sharing an image.
    """
    lock = read_lock(base.lock_path)
    declaration = base.identity.digest
    reusable = await _reusable(
        engine,
        realization=locked_realization(lock, declaration, platform),
        rebuild=rebuild,
        label=f"Base '{base.identity.name}'",
    )
    with tempfile.TemporaryDirectory(prefix="runlab-build-inputs-") as scratch:
        contexts, resolved_inputs = _build_contexts(base, Path(scratch))
        if reusable is not None:
            return reusable, resolved_inputs
        tag = _tag("runlab-base", digest_values(declaration, platform))
        realization = await engine.build(
            BuildRequest(context=base.root, tag=tag, build_contexts=contexts)
        )
    write_lock(
        base.lock_path, record_realization(lock, declaration, platform, realization)
    )
    return realization, resolved_inputs


async def _apply_overlay(
    engine: DockerEngine,
    /,
    *,
    overlay: OverlayDeclaration,
    image: str,
    rebuild: bool,
) -> tuple[str, OverlayBinding]:
    if overlay.definition.layer is None:
        return image, OverlayBinding(
            name=overlay.identity.name,
            declaration=overlay.identity.digest,
            realization=_content_realization(overlay),
            kind=RealizationKind.CONTENT,
        )
    realization = await _resolve_layer(
        engine, overlay=overlay, image=image, rebuild=rebuild
    )
    return realization, OverlayBinding(
        name=overlay.identity.name,
        declaration=overlay.identity.digest,
        realization=realization,
        kind=RealizationKind.IMAGE,
    )


async def _resolve_layer(
    engine: DockerEngine,
    /,
    *,
    overlay: OverlayDeclaration,
    image: str,
    rebuild: bool,
) -> str:
    lock = read_lock(overlay.lock_path)
    declaration = overlay.identity.digest
    reusable = await _reusable(
        engine,
        realization=locked_realization(lock, declaration, image),
        rebuild=rebuild,
        label=f"Overlay '{overlay.identity.name}'",
    )
    if reusable is not None:
        return reusable
    tag = _tag("runlab-overlay", digest_values(declaration, image))
    realization = await engine.build(
        BuildRequest(
            context=overlay.root,
            tag=tag,
            dockerfile=overlay.layer_path,
            build_args={"BASE_IMAGE": await engine.ensure_reference(image)},
        )
    )
    write_lock(
        overlay.lock_path, record_realization(lock, declaration, image, realization)
    )
    return realization


async def _reusable(
    engine: DockerEngine,
    /,
    *,
    realization: str | None,
    rebuild: bool,
    label: str,
) -> str | None:
    if realization is None:
        return None
    if await engine.image_id(realization) is not None:
        return realization
    if not rebuild:
        message = (
            f"{label} is locked to a realization that is no longer available. "
            "Rebuilding produces an environment that is not comparable with "
            "earlier Runs; pass --rebuild to accept that."
        )
        raise RealizationError(message)
    return None


def _overlay_mounts(overlay: OverlayDeclaration) -> list[HostMount]:
    return [
        HostMount(source=overlay.root / mount.source, target=mount.target)
        for mount in overlay.definition.mounts
    ]


def _content_realization(overlay: OverlayDeclaration) -> str:
    """Address a mount-only Overlay by the content it supplies.

    Mount content is independent of the Base it accompanies, so its realization
    carries no image identity and stays stable across Bases.
    """
    parts: list[str] = []
    for mount in overlay.definition.mounts:
        digest, _kind = identify(overlay.root / mount.source)
        parts.extend([mount.target, digest])
    parts.extend(
        f"{name}={value}" for name, value in sorted(overlay.definition.env.items())
    )
    if overlay.definition.network is not None:
        parts.extend(["network", overlay.definition.network])
    return digest_values(*parts)


def _build_contexts(
    base: BaseDeclaration, scratch: Path
) -> tuple[dict[str, Path], list[ResolvedBuildInput]]:
    contexts: dict[str, Path] = {}
    resolved: list[ResolvedBuildInput] = []
    for index, request in enumerate(base.definition.build_inputs):
        source = environment_build_path(request.source_env)
        prepared = (
            source
            if not request.include
            else _snapshot(source, request.include, scratch / str(index))
        )
        digest = digest_directory(prepared) if prepared.is_dir() else None
        if digest is None:
            message = f"build input '{request.name}' must be a directory"
            raise DeclarationError(message)
        contexts[request.name] = prepared
        resolved.append(
            ResolvedBuildInput(name=request.name, digest=digest, kind="directory")
        )
    return contexts, resolved


def _snapshot(source: Path, includes: Sequence[str], destination: Path) -> Path:
    destination.mkdir(parents=True)
    for value in includes:
        relative = Path(value)
        if relative.is_absolute() or ".." in relative.parts:
            message = f"a build input include must be a relative path: {value}"
            raise DeclarationError(message)
        entry = source / relative
        target = destination / relative
        if entry.is_dir():
            shutil.copytree(entry, target, symlinks=True)
        elif entry.is_file():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(entry, target, follow_symlinks=False)
        else:
            message = f"build input include does not exist: {entry}"
            raise DeclarationError(message)
    return destination


def _tag(prefix: str, identity: str) -> str:
    return f"{prefix}:{identity.removeprefix('sha256:')[:16]}"
