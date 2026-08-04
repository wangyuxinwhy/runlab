"""Loading and validating Base, Overlay, and Task sources.

This package owns what a declaration directory must contain and what its
identity covers. It knows nothing about how a declaration is later built or
executed.
"""

import json
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

from pydantic import ValidationError

from runlab.core.digest import digest_directory, digest_file
from runlab.core.errors import DeclarationError
from runlab.core.models import (
    BaseDefinition,
    DeclarationIdentity,
    OverlayDefinition,
    ProtocolModel,
    TaskDefinition,
)

BASE_LOCK_NAME = "base.lock"
OVERLAY_LOCK_NAME = "overlay.lock"

# A lock file records the realization built from a declaration, so including it
# in that declaration's digest would make the digest change as a consequence of
# recording itself.
_BASE_EXCLUDED = frozenset({BASE_LOCK_NAME})
_OVERLAY_EXCLUDED = frozenset({OVERLAY_LOCK_NAME})


@dataclass(frozen=True, slots=True)
class BaseDeclaration:
    root: Path
    definition: BaseDefinition
    identity: DeclarationIdentity

    @property
    def dockerfile(self) -> Path:
        return self.root / "Dockerfile"

    @property
    def lock_path(self) -> Path:
        return self.root / BASE_LOCK_NAME


@dataclass(frozen=True, slots=True)
class OverlayDeclaration:
    root: Path
    definition: OverlayDefinition
    identity: DeclarationIdentity

    @property
    def lock_path(self) -> Path:
        return self.root / OVERLAY_LOCK_NAME

    @property
    def layer_path(self) -> Path | None:
        layer = self.definition.layer
        return None if layer is None else self.root / layer


@dataclass(frozen=True, slots=True)
class TaskDeclaration:
    root: Path
    definition: TaskDefinition
    identity: DeclarationIdentity

    @property
    def instruction(self) -> Path:
        return self.root / "task.md"

    @property
    def workspace(self) -> Path | None:
        candidate = self.root / "workspace"
        return candidate if candidate.is_dir() else None

    @property
    def instruction_digest(self) -> str:
        return digest_file(self.instruction)


def load_base(path: Path, /) -> BaseDeclaration:
    root = _directory(path, "Base")
    _require_file(root / "Dockerfile")
    definition = _load_optional(root / "base.json", BaseDefinition, BaseDefinition())
    return BaseDeclaration(
        root=root,
        definition=definition,
        identity=DeclarationIdentity(
            name=definition.name or root.name,
            digest=digest_directory(root, exclude=_BASE_EXCLUDED),
        ),
    )


def load_overlay(path: Path, /) -> OverlayDeclaration:
    root = _directory(path, "Overlay")
    definition = _load_required(root / "overlay.json", OverlayDefinition)
    declaration = OverlayDeclaration(
        root=root,
        definition=definition,
        identity=DeclarationIdentity(
            name=definition.name or root.name,
            digest=digest_directory(root, exclude=_OVERLAY_EXCLUDED),
        ),
    )
    _validate_overlay_sources(declaration)
    return declaration


def effective_overlays(
    overlays: Sequence[OverlayDeclaration], /
) -> list[OverlayDeclaration]:
    """Drop Overlays that change nothing.

    An empty Overlay and an absent Overlay are the same environment, so keeping
    the empty one would produce two declarations with one realization and make
    comparability judgment ambiguous.
    """
    return [item for item in overlays if not item.definition.is_empty]


def load_task(path: Path, /) -> TaskDeclaration:
    root = _directory(path, "Task")
    _require_file(root / "task.md")
    if (root / "Dockerfile").exists():
        message = f"a Task must not contain a Dockerfile: {root}"
        raise DeclarationError(message)
    definition = _load_optional(root / "task.json", TaskDefinition, TaskDefinition())
    return TaskDeclaration(
        root=root,
        definition=definition,
        identity=DeclarationIdentity(
            name=definition.name or root.name,
            digest=digest_directory(root),
        ),
    )


def _validate_overlay_sources(declaration: OverlayDeclaration) -> None:
    layer = declaration.layer_path
    if layer is not None and not layer.is_file():
        message = f"Overlay layer file is missing: {layer}"
        raise DeclarationError(message)
    for mount in declaration.definition.mounts:
        source = declaration.root / mount.source
        if not source.exists():
            message = f"Overlay mount source is missing: {source}"
            raise DeclarationError(message)


def _directory(path: Path, label: str) -> Path:
    try:
        root = path.expanduser().resolve(strict=True)
    except FileNotFoundError as error:
        message = f"{label} directory does not exist: {path}"
        raise DeclarationError(message) from error
    if not root.is_dir():
        message = f"{label} path is not a directory: {path}"
        raise DeclarationError(message)
    return root


def _require_file(path: Path) -> None:
    if not path.is_file():
        message = f"required file is missing: {path}"
        raise DeclarationError(message)


def _load_optional[ModelT: ProtocolModel](
    path: Path, model_type: type[ModelT], default: ModelT
) -> ModelT:
    if not path.exists():
        return default
    return _load_required(path, model_type)


def _load_required[ModelT: ProtocolModel](
    path: Path, model_type: type[ModelT]
) -> ModelT:
    _require_file(path)
    try:
        return model_type.model_validate(json.loads(path.read_text()))
    except (json.JSONDecodeError, ValidationError) as error:
        message = f"invalid declaration {path}: {error}"
        raise DeclarationError(message) from error
