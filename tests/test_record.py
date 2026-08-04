from pathlib import Path

from runlab.record.storage import (
    RunStorage,
    SnapshotSource,
    collect_run_storage,
    prepare_run_storage,
    remove_scratch,
)


def prepare(tmp_path: Path, *, runtime_logs: bool = False) -> RunStorage:
    base_root = tmp_path / "base"
    base_root.mkdir()
    (base_root / "Dockerfile").write_text("FROM scratch\n")
    task_root = tmp_path / "task"
    (task_root / "workspace").mkdir(parents=True)
    (task_root / "task.md").write_text("Do the work.\n")
    (task_root / "workspace" / "seed.txt").write_text("seed\n")
    output = tmp_path / "runs"
    output.mkdir()
    return prepare_run_storage(
        output,
        run_id="run:abc",
        task_root=task_root,
        snapshots=[
            SnapshotSource(relative="base", root=base_root),
            SnapshotSource(relative="task", root=task_root),
        ],
        collect_runtime_logs=runtime_logs,
    )


def test_declarations_are_archived_beside_the_record(tmp_path: Path) -> None:
    storage = prepare(tmp_path)

    assert (storage.inputs / "base" / "Dockerfile").is_file()
    assert (storage.inputs / "task" / "task.md").is_file()
    assert (storage.inputs / "task" / "workspace" / "seed.txt").is_file()
    remove_scratch(storage)


def test_the_initial_workspace_is_copied_rather_than_shared(tmp_path: Path) -> None:
    storage = prepare(tmp_path)

    assert (storage.workspace / "seed.txt").read_text() == "seed\n"
    assert storage.workspace.parent == storage.scratch_directory
    remove_scratch(storage)


def test_a_symbolic_link_is_not_retained_as_an_artifact(tmp_path: Path) -> None:
    storage = prepare(tmp_path)
    (storage.artifacts / "link").symlink_to(tmp_path / "elsewhere")

    snapshot = collect_run_storage(
        storage, require_artifacts=False, require_runtime_logs=False
    )

    assert snapshot.artifacts.error is not None
    assert "symbolic link" in snapshot.artifacts.error
    remove_scratch(storage)


def test_an_empty_artifact_directory_fails_a_successful_run(tmp_path: Path) -> None:
    storage = prepare(tmp_path)

    snapshot = collect_run_storage(
        storage, require_artifacts=True, require_runtime_logs=False
    )

    assert snapshot.artifacts.error == "no artifact files were produced"
    remove_scratch(storage)


def test_declared_but_empty_native_logs_fail_collection(tmp_path: Path) -> None:
    storage = prepare(tmp_path, runtime_logs=True)
    (storage.artifacts / "report.md").write_text("done\n")

    snapshot = collect_run_storage(
        storage, require_artifacts=True, require_runtime_logs=True
    )

    assert snapshot.logs.error == "the Agent runtime produced no native logs"
    assert snapshot.artifact_bytes > 0
    remove_scratch(storage)
