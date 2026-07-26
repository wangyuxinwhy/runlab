from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from pydantic import ValidationError

from runlab.errors import DefinitionError
from runlab.identity import digest_directory, digest_file
from runlab.models import (
    EnvironmentDefinition,
    ExperimentDefinition,
    PackageIdentity,
    ProtocolModel,
    TaskDefinition,
)


@dataclass(frozen=True, slots=True)
class EnvironmentPackage:
    root: Path
    definition: EnvironmentDefinition
    identity: PackageIdentity


@dataclass(frozen=True, slots=True)
class TaskPackage:
    root: Path
    definition: TaskDefinition
    identity: PackageIdentity
    instruction_digest: str


@dataclass(frozen=True, slots=True)
class ExperimentPackage:
    root: Path
    definition: ExperimentDefinition
    environments: tuple[EnvironmentPackage, ...]
    tasks: tuple[TaskPackage, ...]


def load_environment(path: Path) -> EnvironmentPackage:
    root = _directory(path, "Environment")
    _required_file(root / "Dockerfile")
    definition = _load_optional_model(
        root / "environment.json",
        EnvironmentDefinition,
        EnvironmentDefinition(),
    )
    name = definition.name or root.name
    return EnvironmentPackage(
        root=root,
        definition=definition,
        identity=PackageIdentity(name=name, digest=digest_directory(root)),
    )


def load_task(path: Path) -> TaskPackage:
    root = _directory(path, "Task")
    task_file = root / "task.md"
    _required_file(task_file)
    if (root / "Dockerfile").exists():
        msg = f"Task must not contain a Dockerfile: {root}"
        raise DefinitionError(msg)
    definition = _load_optional_model(
        root / "task.json",
        TaskDefinition,
        TaskDefinition(),
    )
    name = definition.name or root.name
    return TaskPackage(
        root=root,
        definition=definition,
        identity=PackageIdentity(name=name, digest=digest_directory(root)),
        instruction_digest=digest_file(task_file),
    )


def load_experiment(path: Path) -> ExperimentPackage:
    root = _directory(path, "Experiment")
    definition = _load_model(root / "experiment.json", ExperimentDefinition)
    environments = tuple(
        load_environment(child)
        for child in _package_directories(root / "environments", "Environment")
    )
    tasks = tuple(
        load_task(child) for child in _package_directories(root / "tasks", "Task")
    )
    return ExperimentPackage(
        root=root,
        definition=definition,
        environments=environments,
        tasks=tasks,
    )


def _directory(path: Path, label: str) -> Path:
    try:
        root = path.expanduser().resolve(strict=True)
    except FileNotFoundError as error:
        msg = f"{label} directory does not exist: {path}"
        raise DefinitionError(msg) from error
    if not root.is_dir():
        msg = f"{label} path is not a directory: {path}"
        raise DefinitionError(msg)
    return root


def _required_file(path: Path) -> None:
    if not path.is_file():
        msg = f"required file is missing: {path}"
        raise DefinitionError(msg)


def _package_directories(root: Path, label: str) -> list[Path]:
    root = _directory(root, f"{label} collection")
    packages = sorted(
        (path for path in root.iterdir() if path.is_dir()), key=lambda path: path.name
    )
    if not packages:
        msg = f"{label} collection is empty: {root}"
        raise DefinitionError(msg)
    return packages


def _load_optional_model[ModelT: ProtocolModel](
    path: Path,
    model_type: type[ModelT],
    default: ModelT,
) -> ModelT:
    if not path.exists():
        return default
    return _load_model(path, model_type)


def _load_model[ModelT: ProtocolModel](
    path: Path,
    model_type: type[ModelT],
) -> ModelT:
    _required_file(path)
    try:
        value = json.loads(path.read_text())
        return model_type.model_validate(value)
    except (json.JSONDecodeError, ValidationError) as error:
        msg = f"invalid definition {path}: {error}"
        raise DefinitionError(msg) from error
