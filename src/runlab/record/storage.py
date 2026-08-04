"""Run directory layout, input snapshots, and content-addressed manifests.

This package owns what a Run directory contains and what makes it complete. A
Run is an asset, so the declarations that produced it are copied in rather than
referenced: a digest proves two things differ but reconstructs neither.
"""

import shutil
import stat
import tempfile
from dataclasses import dataclass
from pathlib import Path

from runlab.core.digest import digest_file
from runlab.core.models import FileSet, Logs, StoredFile


@dataclass(frozen=True, slots=True)
class SnapshotSource:
    """One declaration directory to archive under `inputs/`."""

    relative: str
    root: Path


@dataclass(frozen=True, slots=True)
class RunStorage:
    run_directory: Path
    scratch_directory: Path
    workspace: Path
    inputs: Path
    artifacts: Path
    logs: Path
    runtime_logs: Path | None
    stdout: Path
    stderr: Path


@dataclass(frozen=True, slots=True)
class CollectionSnapshot:
    inputs: FileSet
    artifacts: FileSet
    logs: Logs
    workspace_bytes: int
    artifact_bytes: int
    log_bytes: int


def prepare_run_storage(
    output_root: Path,
    /,
    *,
    run_id: str,
    task_root: Path,
    snapshots: list[SnapshotSource],
    collect_runtime_logs: bool,
) -> RunStorage:
    scratch_directory, workspace = _prepare_workspace(task_root)
    run_directory = output_root / run_id.replace(":", "-")
    try:
        run_directory.mkdir()
        inputs = run_directory / "inputs"
        artifacts = run_directory / "artifacts"
        logs = run_directory / "logs"
        for directory in (inputs, artifacts, logs):
            directory.mkdir()
        runtime_logs = logs / "runtime" if collect_runtime_logs else None
        if runtime_logs is not None:
            runtime_logs.mkdir()
        for snapshot in snapshots:
            destination = inputs / snapshot.relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(snapshot.root, destination, symlinks=True)
        (logs / "task.md").write_bytes((task_root / "task.md").read_bytes())
        stdout = logs / "stdout.log"
        stderr = logs / "stderr.log"
        stdout.touch()
        stderr.touch()
        return RunStorage(
            run_directory=run_directory,
            scratch_directory=scratch_directory,
            workspace=workspace,
            inputs=inputs,
            artifacts=artifacts,
            logs=logs,
            runtime_logs=runtime_logs,
            stdout=stdout,
            stderr=stderr,
        )
    except OSError:
        shutil.rmtree(scratch_directory, ignore_errors=True)
        raise


def collect_run_storage(
    storage: RunStorage, /, *, require_artifacts: bool, require_runtime_logs: bool
) -> CollectionSnapshot:
    input_files, input_errors = _manifest(storage.inputs, storage.run_directory)
    artifact_files, artifact_errors = _manifest(
        storage.artifacts, storage.run_directory
    )
    log_files, log_errors = _manifest(storage.logs, storage.run_directory)
    if require_artifacts and not artifact_files:
        artifact_errors.append("no artifact files were produced")
    if require_runtime_logs and not _has_runtime_logs(log_files):
        log_errors.append("the Agent runtime produced no native logs")
    return CollectionSnapshot(
        inputs=FileSet(
            root="inputs", files=input_files, error=_error_message(input_errors)
        ),
        artifacts=FileSet(
            root="artifacts",
            files=artifact_files,
            error=_error_message(artifact_errors),
        ),
        logs=Logs(
            runtime="logs/runtime" if storage.runtime_logs is not None else None,
            files=log_files,
            error=_error_message(log_errors),
        ),
        workspace_bytes=_directory_size(storage.workspace),
        artifact_bytes=sum(item.size_bytes for item in artifact_files),
        log_bytes=sum(item.size_bytes for item in log_files),
    )


def remove_scratch(storage: RunStorage, /) -> None:
    shutil.rmtree(storage.scratch_directory)


def with_log_error(snapshot: CollectionSnapshot, message: str, /) -> CollectionSnapshot:
    existing = snapshot.logs.error
    error = message if existing is None else f"{existing}; {message}"
    return CollectionSnapshot(
        inputs=snapshot.inputs,
        artifacts=snapshot.artifacts,
        logs=snapshot.logs.model_copy(update={"error": error}),
        workspace_bytes=snapshot.workspace_bytes,
        artifact_bytes=snapshot.artifact_bytes,
        log_bytes=snapshot.log_bytes,
    )


def _manifest(root: Path, run_directory: Path) -> tuple[list[StoredFile], list[str]]:
    files: list[StoredFile] = []
    errors: list[str] = []
    for path in sorted(root.rglob("*")):
        mode = path.lstat().st_mode
        relative = path.relative_to(run_directory).as_posix()
        if stat.S_ISLNK(mode):
            errors.append(f"symbolic link is not retained: {relative}")
        elif stat.S_ISREG(mode):
            files.append(
                StoredFile(
                    path=relative,
                    size_bytes=path.stat().st_size,
                    digest=digest_file(path),
                )
            )
        elif not stat.S_ISDIR(mode):
            errors.append(f"special file is not retained: {relative}")
    return files, errors


def _prepare_workspace(task_root: Path) -> tuple[Path, Path]:
    scratch_directory = Path(tempfile.mkdtemp(prefix="runlab-workspace-"))
    workspace = scratch_directory / "workspace"
    source_workspace = task_root / "workspace"
    try:
        if source_workspace.is_dir():
            shutil.copytree(source_workspace, workspace, symlinks=True)
        else:
            workspace.mkdir()
    except OSError:
        shutil.rmtree(scratch_directory, ignore_errors=True)
        raise
    return scratch_directory, workspace


def _directory_size(root: Path) -> int:
    total = 0
    for path in root.rglob("*"):
        if stat.S_ISREG(path.lstat().st_mode):
            total += path.stat().st_size
    return total


def _has_runtime_logs(files: list[StoredFile]) -> bool:
    return any(item.path.startswith("logs/runtime/") for item in files)


def _error_message(errors: list[str]) -> str | None:
    return "; ".join(errors) if errors else None
