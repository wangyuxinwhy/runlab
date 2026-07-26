from pathlib import Path

from runlab.storage import (
    collect_run_storage,
    prepare_run_storage,
    remove_scratch,
)


def test_symlink_is_not_retained_as_an_artifact(tmp_path: Path) -> None:
    task = tmp_path / "task"
    task.mkdir()
    (task / "task.md").write_text("Do the work.\n")
    (tmp_path / "runs").mkdir()
    storage = prepare_run_storage(
        tmp_path / "runs",
        task,
        "run:test",
        collect_runtime_logs=False,
    )
    (storage.artifacts / "link").symlink_to("/run/credentials/private")

    snapshot = collect_run_storage(
        storage,
        require_artifacts=True,
        require_runtime_logs=False,
    )

    assert snapshot.artifacts.files == []
    assert snapshot.artifacts.error == (
        "symbolic link is not retained: artifacts/link; no artifact files were produced"
    )
    remove_scratch(storage)
