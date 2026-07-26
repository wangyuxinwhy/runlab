from __future__ import annotations

import asyncio
from pathlib import Path
from typing import override

import pytest

from runlab.bindings import InputResolver
from runlab.docker import DockerEngine
from runlab.errors import DefinitionError
from runlab.execution import (
    AcceptedRun,
    ContainerProcess,
    ExecutionFacts,
    Runner,
    RunRequest,
)
from runlab.models import (
    Measurements,
    ProcessResult,
    RunOutcome,
    RunPolicy,
)
from runlab.packages import load_environment, load_task


class CancellationEngine(DockerEngine):
    def __init__(self) -> None:
        self.stopped: list[str] = []
        self.removed: list[str] = []

    async def check(self) -> str:
        return "test"

    async def ensure_image(
        self,
        context: Path,
        tag: str,
        *,
        build_contexts: dict[str, Path] | None = None,
    ) -> str:
        del context, tag, build_contexts
        return "sha256:image"

    async def create(self, arguments: list[str]) -> str:
        del arguments
        return "container-id"

    async def stop(self, container: str) -> None:
        self.stopped.append(container)

    async def remove(self, container: str) -> None:
        self.removed.append(container)


class CancellationRunner(Runner):
    @override
    async def _run_container(
        self,
        request: ContainerProcess,
        /,
    ) -> tuple[ProcessResult, Measurements, RunOutcome]:
        del request
        raise asyncio.CancelledError


class StorageRunner(Runner):
    def __init__(self, engine: DockerEngine, *, write_artifact: bool) -> None:
        super().__init__(engine=engine)
        self.write_artifact = write_artifact
        self.workspace: Path | None = None

    @override
    async def _execute(self, accepted: AcceptedRun) -> ExecutionFacts:
        self.workspace = accepted.storage.workspace
        if self.write_artifact:
            (accepted.storage.artifacts / "report.md").write_text("result\n")
        if accepted.storage.runtime_logs is not None:
            (accepted.storage.runtime_logs / "session.jsonl").write_text("{}\n")
        return ExecutionFacts(
            outcome=RunOutcome.SUCCEEDED,
            process=ProcessResult(exit_code=0),
            measurements=Measurements(),
        )


async def test_accepted_cancellation_preserves_terminal_record(tmp_path: Path) -> None:
    environment_root = tmp_path / "environment"
    environment_root.mkdir()
    (environment_root / "Dockerfile").write_text("FROM scratch\n")
    task_root = tmp_path / "task"
    task_root.mkdir()
    (task_root / "task.md").write_text("Do the work.\n")
    engine = CancellationEngine()
    runner = CancellationRunner(engine=engine)

    record, record_path = await runner.run(
        RunRequest(
            environment=load_environment(environment_root),
            task=load_task(task_root),
            output_root=tmp_path / "runs",
            policy=RunPolicy(timeout_seconds=10),
        )
    )

    assert record.outcome is RunOutcome.COLLECTION_FAILED
    assert record.process.error == "run cancelled by operator"
    assert record_path.is_file()
    assert engine.stopped == [record.run_id.replace("run:", "runlab-")]
    assert engine.removed == [record.run_id.replace("run:", "runlab-")]


async def test_run_retains_only_record_artifacts_and_logs(tmp_path: Path) -> None:
    environment_root = tmp_path / "environment"
    environment_root.mkdir()
    (environment_root / "Dockerfile").write_text("FROM scratch\n")
    (environment_root / "environment.json").write_text(
        '{"logs":{"target":"/runtime/logs"}}'
    )
    task_root = tmp_path / "task"
    (task_root / "workspace").mkdir(parents=True)
    (task_root / "task.md").write_text("Write the report.\n")
    (task_root / "workspace" / "input.txt").write_text("input\n")
    runner = StorageRunner(CancellationEngine(), write_artifact=True)

    record, record_path = await runner.run(
        RunRequest(
            environment=load_environment(environment_root),
            task=load_task(task_root),
            output_root=tmp_path / "runs",
            policy=RunPolicy(timeout_seconds=10),
        )
    )

    assert record.schema_version == 2
    assert record.outcome is RunOutcome.SUCCEEDED
    assert [item.path for item in record.artifacts.files] == ["artifacts/report.md"]
    assert record.logs.runtime == "logs/runtime"
    assert {item.path for item in record.logs.files} == {
        "logs/measurements.jsonl",
        "logs/runtime/session.jsonl",
        "logs/stderr.log",
        "logs/stdout.log",
        "logs/task.md",
    }
    assert record.measurements.workspace_bytes_at_completion == 6
    assert record.measurements.artifact_bytes == 7
    assert not record_path.parent.joinpath("workspace").exists()
    assert runner.workspace is not None
    assert not runner.workspace.exists()


async def test_success_without_artifact_becomes_collection_failure(
    tmp_path: Path,
) -> None:
    environment_root = tmp_path / "environment"
    environment_root.mkdir()
    (environment_root / "Dockerfile").write_text("FROM scratch\n")
    task_root = tmp_path / "task"
    task_root.mkdir()
    (task_root / "task.md").write_text("Do the work.\n")
    runner = StorageRunner(CancellationEngine(), write_artifact=False)

    record, _record_path = await runner.run(
        RunRequest(
            environment=load_environment(environment_root),
            task=load_task(task_root),
            output_root=tmp_path / "runs",
            policy=RunPolicy(timeout_seconds=10),
        )
    )

    assert record.process.exit_code == 0
    assert record.outcome is RunOutcome.COLLECTION_FAILED
    assert record.artifacts.error == "no artifact files were produced"


def test_lark_credential_uses_explicit_private_bundle(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    environment_root = tmp_path / "environment"
    environment_root.mkdir()
    (environment_root / "Dockerfile").write_text("FROM scratch\n")
    (environment_root / "environment.json").write_text(
        '{"credentials":[{"name":"lark"}]}'
    )
    task_root = tmp_path / "task"
    task_root.mkdir()
    (task_root / "task.md").write_text("Do the work.\n")
    credential = tmp_path / "credential"
    credential.mkdir()
    monkeypatch.setenv("RUNLAB_LARK_CREDENTIAL_DIR", str(credential))

    bindings = InputResolver().resolve(
        load_environment(environment_root),
        load_task(task_root),
    )

    assert bindings.credential_mounts[0].source == credential
    assert bindings.credential_mounts[0].target == "/run/credentials/lark-cli"


def test_input_cannot_overlap_artifact_mount(tmp_path: Path) -> None:
    environment_root = tmp_path / "environment"
    environment_root.mkdir()
    (environment_root / "Dockerfile").write_text("FROM scratch\n")
    task_root = tmp_path / "task"
    task_root.mkdir()
    (task_root / "task.md").write_text("Do the work.\n")
    (task_root / "task.json").write_text(
        """
        {
          "inputs": [
            {
              "name": "unsafe",
              "source_env": "UNUSED",
              "target": "/artifacts/input"
            }
          ]
        }
        """
    )

    with pytest.raises(DefinitionError, match="overlapping container mount target"):
        InputResolver().resolve(
            load_environment(environment_root),
            load_task(task_root),
        )
