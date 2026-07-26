from __future__ import annotations

import os
import shutil
import stat
import tempfile
from collections.abc import Iterable
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


class BindingResolver:
    def __init__(self, credential_directory: Path | None = None) -> None:
        self._credential_directory = (
            default_credential_directory()
            if credential_directory is None
            else credential_directory
        )
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
        credential_names: set[str] = set()
        for request in environment.definition.credentials:
            if request.name in credential_names:
                msg = f"duplicate credential name: {request.name}"
                raise DefinitionError(msg)
            _claim_mount_target(request.target, seen_targets)
            source = _credential_source(
                self._credential_directory,
                request.name,
                request.kind,
            )
            credential_mounts.append(HostMount(source=source, target=request.target))
            credentials.append(
                ResolvedCredential(
                    name=request.name,
                    kind=request.kind,
                    target=request.target,
                )
            )
            credential_names.add(request.name)
        return ResolvedBindings(
            input_mounts=input_mounts,
            inputs=inputs,
            build_contexts=build_contexts,
            build_inputs=build_inputs,
            credential_mounts=credential_mounts,
            credentials=credentials,
        )

    def preflight_credentials(
        self,
        environments: Iterable[EnvironmentPackage],
        /,
    ) -> None:
        """Resolve every credential before an Experiment accepts any Run."""

        checked: set[tuple[str, Literal["file", "directory"]]] = set()
        for environment in environments:
            for request in environment.definition.credentials:
                requirement = (request.name, request.kind)
                if requirement in checked:
                    continue
                _credential_source(
                    self._credential_directory,
                    request.name,
                    request.kind,
                )
                checked.add(requirement)

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


def default_credential_directory() -> Path:
    config_home = os.environ.get("XDG_CONFIG_HOME")
    base = Path(config_home) if config_home else Path("~/.config")
    return base.expanduser() / "runlab" / "credentials"


def _credential_source(
    directory: Path | None,
    name: str,
    kind: Literal["file", "directory"],
) -> Path:
    root = _private_credential_root(directory, name)
    try:
        source = (root / name).resolve(strict=True)
    except (OSError, RuntimeError) as error:
        msg = f"credential '{name}' is missing; expected {kind} entry"
        raise DefinitionError(msg) from error
    if kind == "file" and not source.is_file():
        msg = f"credential '{name}' must be a regular file"
        raise DefinitionError(msg)
    if kind == "directory" and not source.is_dir():
        msg = f"credential '{name}' must be a directory"
        raise DefinitionError(msg)
    _require_private_permissions(source, f"credential '{name}'")
    return source


def _private_credential_root(directory: Path | None, name: str) -> Path:
    if directory is None:
        msg = f"credentials directory is required for credential '{name}'"
        raise DefinitionError(msg)
    try:
        root = directory.expanduser().resolve(strict=True)
    except (OSError, RuntimeError) as error:
        msg = "credentials directory does not exist"
        raise DefinitionError(msg) from error
    if not root.is_dir():
        msg = "credentials path must be a directory"
        raise DefinitionError(msg)
    _require_private_permissions(root, "credentials directory")
    return root


def _require_private_permissions(path: Path, label: str) -> None:
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
    except OSError as error:
        msg = f"could not inspect {label}"
        raise DefinitionError(msg) from error
    if mode & (stat.S_IRWXG | stat.S_IRWXO):
        msg = f"{label} must not be accessible by group or others"
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
