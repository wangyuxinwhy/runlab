"""Resolving declared slots to host material without leaking host paths.

Every mount target the container will receive passes through here, so overlap
between an Overlay mount, a credential, a declared input, and the fixed
`/workspace` and `/artifacts` mounts is rejected before a Run is accepted.
"""

import os
import stat
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from runlab.core.digest import digest_directory, digest_file
from runlab.core.errors import DeclarationError
from runlab.core.models import (
    CredentialRequest,
    EntryKind,
    InputRequest,
    ResolvedCredential,
    ResolvedInput,
)


@dataclass(frozen=True, slots=True)
class HostMount:
    """Keeps a private host path outside every public model."""

    source: Path
    target: str


@dataclass(frozen=True, slots=True)
class ResolvedBindings:
    mounts: list[HostMount]
    credentials: list[ResolvedCredential]
    inputs: list[ResolvedInput]


class MountTargets:
    """Claims container paths, rejecting any pair where one contains the other."""

    def __init__(self, reserved: Iterable[str], /) -> None:
        self._claimed: set[str] = set()
        for target in reserved:
            self.claim(target)

    def claim(self, target: str, /) -> None:
        candidate = PurePosixPath(target)
        for existing_value in self._claimed:
            existing = PurePosixPath(existing_value)
            if (
                candidate == existing
                or candidate in existing.parents
                or existing in candidate.parents
            ):
                message = (
                    f"overlapping container mount target: {target} and {existing_value}"
                )
                raise DeclarationError(message)
        self._claimed.add(target)


def default_credential_directory() -> Path:
    config_home = os.environ.get("XDG_CONFIG_HOME")
    base = Path(config_home) if config_home else Path("~/.config")
    return base.expanduser() / "runlab" / "credentials"


def resolve_bindings(
    targets: MountTargets,
    /,
    *,
    credential_requests: Sequence[CredentialRequest],
    input_requests: Sequence[InputRequest],
    credential_directory: Path,
) -> ResolvedBindings:
    mounts: list[HostMount] = []
    credentials: list[ResolvedCredential] = []
    inputs: list[ResolvedInput] = []
    for request in _unique_credentials(credential_requests):
        targets.claim(request.target)
        source = credential_source(
            credential_directory, name=request.name, kind=request.kind
        )
        mounts.append(HostMount(source=source, target=request.target))
        credentials.append(
            ResolvedCredential(
                name=request.name, kind=request.kind, target=request.target
            )
        )
    seen_inputs: set[str] = set()
    for request in input_requests:
        if request.name in seen_inputs:
            message = f"duplicate input name: {request.name}"
            raise DeclarationError(message)
        seen_inputs.add(request.name)
        targets.claim(request.target)
        source = _environment_path(request.source_env, "input")
        digest, kind = identify(source)
        mounts.append(HostMount(source=source, target=request.target))
        inputs.append(
            ResolvedInput(
                name=request.name, digest=digest, kind=kind, target=request.target
            )
        )
    return ResolvedBindings(mounts=mounts, credentials=credentials, inputs=inputs)


def identify(source: Path, /) -> tuple[str, EntryKind]:
    if source.is_file():
        return digest_file(source), "file"
    if source.is_dir():
        return digest_directory(source), "directory"
    message = f"input is not a regular file or directory: {source}"
    raise DeclarationError(message)


def credential_source(directory: Path, /, *, name: str, kind: EntryKind) -> Path:
    root = _private_root(directory, name)
    try:
        source = (root / name).resolve(strict=True)
    except (OSError, RuntimeError) as error:
        message = f"credential '{name}' is missing; expected a {kind} entry"
        raise DeclarationError(message) from error
    if kind == "file" and not source.is_file():
        message = f"credential '{name}' must be a regular file"
        raise DeclarationError(message)
    if kind == "directory" and not source.is_dir():
        message = f"credential '{name}' must be a directory"
        raise DeclarationError(message)
    _require_private(source, f"credential '{name}'")
    return source


def environment_build_path(name: str, /) -> Path:
    return _environment_path(name, "build input")


def _unique_credentials(
    requests: Sequence[CredentialRequest],
) -> list[CredentialRequest]:
    """Collapse the same slot requested by several declarations.

    A Base and an Overlay may both need one credential; requesting it twice at
    the same target is agreement, while two targets for one name is ambiguity.
    """
    by_name: dict[str, CredentialRequest] = {}
    for request in requests:
        existing = by_name.get(request.name)
        if existing is None:
            by_name[request.name] = request
        elif existing != request:
            message = f"conflicting declarations for credential '{request.name}'"
            raise DeclarationError(message)
    return list(by_name.values())


def _environment_path(name: str, label: str) -> Path:
    raw = os.environ.get(name)
    if not raw:
        message = f"required {label} environment variable is not set: {name}"
        raise DeclarationError(message)
    try:
        return Path(raw).expanduser().resolve(strict=True)
    except OSError as error:
        message = f"{label} path from {name} does not exist"
        raise DeclarationError(message) from error


def _private_root(directory: Path, name: str) -> Path:
    try:
        root = directory.expanduser().resolve(strict=True)
    except (OSError, RuntimeError) as error:
        message = f"the credentials directory does not exist, needed for '{name}'"
        raise DeclarationError(message) from error
    if not root.is_dir():
        message = "the credentials path must be a directory"
        raise DeclarationError(message)
    _require_private(root, "the credentials directory")
    return root


def _require_private(path: Path, label: str) -> None:
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
    except OSError as error:
        message = f"could not inspect {label}"
        raise DeclarationError(message) from error
    if mode & (stat.S_IRWXG | stat.S_IRWXO):
        message = f"{label} must not be accessible by group or others"
        raise DeclarationError(message)
