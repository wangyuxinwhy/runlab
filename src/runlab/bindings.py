from __future__ import annotations

import os
import shutil
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Literal

from runlab.errors import DefinitionError
from runlab.identity import digest_directory, digest_file
from runlab.models import ResolvedBuildInput, ResolvedCredential, ResolvedInput
from runlab.packages import EnvironmentPackage, TaskPackage


@dataclass(frozen=True, slots=True)
class HostMount:
    """Keep private host paths outside public Run models."""

    source: Path
    target: str


@dataclass(frozen=True, slots=True)
class HostBuildInput:
    name: str
    source: Path


@dataclass(frozen=True, slots=True)
class ResolvedBindings:
    input_mounts: list[HostMount]
    inputs: list[ResolvedInput]
    build_contexts: list[HostBuildInput]
    build_inputs: list[ResolvedBuildInput]
    credential_mounts: list[HostMount]
    credentials: list[ResolvedCredential]


class InputResolver:
    def __init__(self) -> None:
        self._digests: dict[
            Path,
            tuple[str, Literal["file", "directory"]],
        ] = {}
        self._snapshots: dict[tuple[Path, tuple[str, ...]], Path] = {}
        self._temporary = tempfile.TemporaryDirectory(prefix="runlab-build-inputs-")

    def resolve(
        self, environment: EnvironmentPackage, task: TaskPackage
    ) -> ResolvedBindings:
        input_mounts: list[HostMount] = []
        inputs: list[ResolvedInput] = []
        seen_names: set[str] = set()
        seen_targets: set[str] = set()
        _claim_mount_target("/workspace", seen_targets)
        _claim_mount_target("/artifacts", seen_targets)
        if environment.definition.logs is not None:
            _claim_mount_target(environment.definition.logs.target, seen_targets)
        for request in [*environment.definition.inputs, *task.definition.inputs]:
            if request.name in seen_names:
                msg = f"duplicate input name: {request.name}"
                raise DefinitionError(msg)
            _claim_mount_target(request.target, seen_targets)
            source = _input_source(request.source_env)
            digest, kind = self._identify(source)
            input_mounts.append(HostMount(source=source, target=request.target))
            inputs.append(
                ResolvedInput(
                    name=request.name,
                    digest=digest,
                    target=request.target,
                    kind=kind,
                )
            )
            seen_names.add(request.name)

        build_contexts: list[HostBuildInput] = []
        build_inputs: list[ResolvedBuildInput] = []
        for request in environment.definition.build_inputs:
            if request.name in seen_names:
                msg = f"duplicate input name: {request.name}"
                raise DefinitionError(msg)
            source = _environment_source(request.source_env)
            snapshot = self._snapshot(source, request.include)
            digest, kind = self._identify(snapshot)
            build_contexts.append(HostBuildInput(name=request.name, source=snapshot))
            build_inputs.append(
                ResolvedBuildInput(
                    name=request.name,
                    digest=digest,
                    kind=kind,
                )
            )
            seen_names.add(request.name)

        credential_mounts: list[HostMount] = []
        credentials: list[ResolvedCredential] = []
        for request in environment.definition.credentials:
            _claim_mount_target(request.target, seen_targets)
            source = _credential_source(request.name)
            credential_mounts.append(HostMount(source=source, target=request.target))
            credentials.append(
                ResolvedCredential(name=request.name, target=request.target)
            )
        return ResolvedBindings(
            input_mounts=input_mounts,
            inputs=inputs,
            build_contexts=build_contexts,
            build_inputs=build_inputs,
            credential_mounts=credential_mounts,
            credentials=credentials,
        )

    def _identify(
        self,
        source: Path,
    ) -> tuple[str, Literal["file", "directory"]]:
        cached = self._digests.get(source)
        if cached is not None:
            return cached
        if source.is_file():
            identity = (digest_file(source), "file")
        elif source.is_dir():
            identity = (digest_directory(source), "directory")
        else:
            msg = f"input is not a regular file or directory: {source}"
            raise DefinitionError(msg)
        self._digests[source] = identity
        return identity

    def _snapshot(self, source: Path, includes: tuple[str, ...]) -> Path:
        if not includes:
            return source
        key = (source, includes)
        if cached := self._snapshots.get(key):
            return cached
        destination = Path(self._temporary.name) / str(len(self._snapshots))
        destination.mkdir()
        for value in includes:
            relative = Path(value)
            if relative.is_absolute() or ".." in relative.parts:
                msg = f"build input include must be a relative path: {value}"
                raise DefinitionError(msg)
            _copy_snapshot_entry(source / relative, destination / relative)
        self._snapshots[key] = destination
        return destination


def _input_source(name: str) -> Path:
    raw_source = os.environ.get(name)
    if not raw_source:
        msg = f"required input environment variable is not set: {name}"
        raise DefinitionError(msg)
    return Path(raw_source).expanduser().resolve(strict=True)


def _credential_source(name: str) -> Path:
    if name == "codex":
        codex_home = Path(os.environ.get("CODEX_HOME", "~/.codex")).expanduser()
        source = (codex_home / "auth.json").resolve(strict=True)
        if not source.is_file():
            msg = "Codex credential is not a regular file"
            raise DefinitionError(msg)
        return source
    if name == "lark":
        raw_source = os.environ.get("RUNLAB_LARK_CREDENTIAL_DIR")
        if not raw_source:
            msg = (
                "required Lark credential bundle is not set: RUNLAB_LARK_CREDENTIAL_DIR"
            )
            raise DefinitionError(msg)
        source = Path(raw_source).expanduser().resolve(strict=True)
        if not source.is_dir():
            msg = "Lark credential is not a directory"
            raise DefinitionError(msg)
        return source
    msg = f"unsupported credential provider: {name}"
    raise DefinitionError(msg)


def _environment_source(name: str) -> Path:
    raw_source = os.environ.get(name)
    if not raw_source:
        msg = f"required build input environment variable is not set: {name}"
        raise DefinitionError(msg)
    return Path(raw_source).expanduser().resolve(strict=True)


def _copy_snapshot_entry(source: Path, destination: Path) -> None:
    if source.is_dir():
        shutil.copytree(source, destination, symlinks=True)
        return
    if source.is_file():
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination, follow_symlinks=False)
        return
    msg = f"build input include does not exist: {source}"
    raise DefinitionError(msg)


def _claim_mount_target(target: str, seen: set[str]) -> None:
    candidate = PurePosixPath(target)
    for value in seen:
        existing = PurePosixPath(value)
        if (
            candidate == existing
            or candidate in existing.parents
            or existing in candidate.parents
        ):
            msg = f"overlapping container mount target: {target} and {value}"
            raise DefinitionError(msg)
    seen.add(target)
