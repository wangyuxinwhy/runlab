"""Executing one Run and preserving its terminal record.

Acceptance is the boundary that matters here. Anything that fails before a Run
is accepted produces no record, because nothing was executed; anything that
fails after acceptance produces a terminal record, because a lost failure is a
survivorship bias no downstream analysis can detect.
"""

import asyncio
import json
import logging
from contextlib import suppress
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from runlab.container.engine import DockerEngine
from runlab.core.digest import new_id
from runlab.core.errors import ExecutionError, RunLabError
from runlab.core.models import (
    ContainerResult,
    FileSet,
    Logs,
    Measurements,
    ProcessResult,
    RunOutcome,
    RunPolicy,
    RunRecord,
    RunSpec,
)
from runlab.declarations.loading import (
    BaseDeclaration,
    OverlayDeclaration,
    TaskDeclaration,
    effective_overlays,
)
from runlab.engine.binding import (
    HostMount,
    MountTargets,
    default_credential_directory,
    resolve_bindings,
)
from runlab.engine.resolution import ResolvedEnvironment, resolve_environment
from runlab.record.storage import (
    CollectionSnapshot,
    RunStorage,
    SnapshotSource,
    collect_run_storage,
    prepare_run_storage,
    remove_scratch,
    with_log_error,
)
from runlab.usage.collection import collect_model_usage

_LOGGER = logging.getLogger(__name__)


@dataclass(frozen=True, slots=True)
class RunRequest:
    base: BaseDeclaration
    overlays: list[OverlayDeclaration]
    task: TaskDeclaration
    output_root: Path
    policy: RunPolicy
    credential_directory: Path | None = None
    rebuild: bool = False


@dataclass(frozen=True, slots=True)
class _AcceptedRun:
    request: RunRequest
    run_id: str
    storage: RunStorage
    spec: RunSpec
    environment: ResolvedEnvironment
    mounts: list[HostMount]
    task_bytes: bytes
    created_at: datetime


@dataclass(frozen=True, slots=True)
class _ContainerRun:
    """What the container engine observed, before it is interpreted."""

    name: str
    container: ContainerResult
    client_exit: int
    timed_out: bool
    wall_seconds: float


@dataclass(frozen=True, slots=True)
class _ExecutionFacts:
    outcome: RunOutcome = RunOutcome.SETUP_FAILED
    process: ProcessResult = field(default_factory=ProcessResult)
    container: ContainerResult = field(default_factory=ContainerResult)
    measurements: Measurements = field(default_factory=Measurements)
    container_name: str | None = None


class Runner:
    def __init__(self, engine: DockerEngine | None = None) -> None:
        self.engine = engine or DockerEngine()

    async def run(self, request: RunRequest, /) -> tuple[RunRecord, Path]:
        accepted = await self._accept(request)
        facts = await self._execute(accepted)
        if facts.container_name is not None:
            with suppress(ExecutionError):
                await self.engine.remove(facts.container_name)
        collection = _collect(accepted, facts.outcome)
        try:
            remove_scratch(accepted.storage)
        except OSError:
            collection = with_log_error(
                collection, "could not remove the temporary workspace"
            )
        record = _build_record(accepted, facts, collection)
        record_path = accepted.storage.run_directory / "run.json"
        _write_json(record_path, record.model_dump(mode="json"))
        _LOGGER.info("finish %s outcome=%s", record.run_id, record.outcome)
        return record, record_path

    async def _accept(self, request: RunRequest) -> _AcceptedRun:
        output_root = request.output_root.expanduser().resolve()
        output_root.mkdir(parents=True, exist_ok=True)
        overlays = effective_overlays(request.overlays)
        environment = await resolve_environment(
            self.engine,
            base=request.base,
            overlays=overlays,
            rebuild=request.rebuild,
        )
        targets = MountTargets(_reserved_targets(request, environment))
        for mount in environment.mounts:
            targets.claim(mount.target)
        bindings = resolve_bindings(
            targets,
            credential_requests=environment.credential_requests,
            input_requests=[
                *environment.input_requests,
                *request.task.definition.inputs,
            ],
            credential_directory=(
                request.credential_directory or default_credential_directory()
            ),
        )
        spec = RunSpec(
            environment=environment.spec,
            task=request.task.identity,
            policy=request.policy,
            credentials=bindings.credentials,
            inputs=bindings.inputs,
            build_inputs=environment.build_inputs,
        )
        run_id = new_id("run")
        storage = prepare_run_storage(
            output_root,
            run_id=run_id,
            task_root=request.task.root,
            snapshots=_snapshot_sources(request.base, overlays, request.task),
            collect_runtime_logs=request.base.definition.logs is not None,
        )
        return _AcceptedRun(
            request=request,
            run_id=run_id,
            storage=storage,
            spec=spec,
            environment=environment,
            mounts=[*environment.mounts, *bindings.mounts],
            task_bytes=request.task.instruction.read_bytes(),
            created_at=datetime.now(UTC),
        )

    async def _execute(self, accepted: _AcceptedRun) -> _ExecutionFacts:
        container_name = f"runlab-{accepted.run_id.removeprefix('run:')}"
        started = asyncio.get_running_loop().time()
        container = ContainerResult()
        try:
            engine_version = await self.engine.version()
            container_id = await self.engine.create(_create_arguments(accepted))
            _LOGGER.info(
                "start %s base=%s task=%s",
                accepted.run_id,
                accepted.spec.environment.base.name,
                accepted.spec.task.name,
            )
            container = ContainerResult(
                engine_version=engine_version,
                container_id=container_id,
                started_at=datetime.now(UTC),
            )
            client_exit, timed_out = await self.engine.start_attached(
                container_name,
                stdin_bytes=accepted.task_bytes,
                stdout_path=accepted.storage.stdout,
                stderr_path=accepted.storage.stderr,
                timeout_seconds=accepted.request.policy.timeout_seconds,
            )
            return await self._terminal_facts(
                accepted,
                _ContainerRun(
                    name=container_name,
                    container=container,
                    client_exit=client_exit,
                    timed_out=timed_out,
                    wall_seconds=asyncio.get_running_loop().time() - started,
                ),
            )
        except asyncio.CancelledError:
            with suppress(ExecutionError):
                await self.engine.stop(container_name)
            return _ExecutionFacts(
                outcome=RunOutcome.COLLECTION_FAILED,
                process=ProcessResult(error="the Run was cancelled by the operator"),
                container=container,
                measurements=Measurements(
                    wall_seconds=asyncio.get_running_loop().time() - started
                ),
                container_name=container_name,
            )
        except (RunLabError, OSError) as error:
            return _ExecutionFacts(
                process=ProcessResult(error=str(error)),
                container=container,
                measurements=Measurements(
                    wall_seconds=asyncio.get_running_loop().time() - started
                ),
                container_name=container_name,
            )

    async def _terminal_facts(
        self, accepted: _AcceptedRun, run: _ContainerRun, /
    ) -> _ExecutionFacts:
        state = await self.engine.inspect_state(run.name)
        usage, usage_error = collect_model_usage(
            accepted.request.base.definition.output_protocol,
            stdout_path=accepted.storage.stdout,
            runtime_logs_path=accepted.storage.runtime_logs,
        )
        process = ProcessResult(
            exit_code=state.exit_code, oom_killed=state.oom_killed, error=state.error
        )
        outcome = _outcome_from_state(
            timed_out=run.timed_out,
            oom_killed=state.oom_killed,
            exit_code=state.exit_code,
        )
        if run.client_exit != 0 and outcome is RunOutcome.SUCCEEDED:
            process = process.model_copy(
                update={"error": "the attached Docker client failed"}
            )
            outcome = RunOutcome.COLLECTION_FAILED
        if usage_error is not None and outcome is RunOutcome.SUCCEEDED:
            # Usage is a core asset, so a Run that cannot report it is
            # incomplete for the same reason a Run without artifacts is.
            outcome = RunOutcome.COLLECTION_FAILED
        return _ExecutionFacts(
            outcome=outcome,
            process=process,
            container=run.container,
            measurements=Measurements(
                wall_seconds=run.wall_seconds,
                model_usage=usage,
                model_usage_error=usage_error,
            ),
            container_name=run.name,
        )


def _outcome_from_state(
    *, timed_out: bool, oom_killed: bool, exit_code: int
) -> RunOutcome:
    if timed_out:
        return RunOutcome.TIMED_OUT
    if oom_killed:
        return RunOutcome.OOM_KILLED
    if exit_code == 0:
        return RunOutcome.SUCCEEDED
    return RunOutcome.AGENT_FAILED


def _reserved_targets(
    request: RunRequest, environment: ResolvedEnvironment
) -> list[str]:
    del environment
    reserved = ["/workspace", "/artifacts"]
    logs = request.base.definition.logs
    if logs is not None:
        reserved.append(logs.target)
    return reserved


def _snapshot_sources(
    base: BaseDeclaration,
    overlays: list[OverlayDeclaration],
    task: TaskDeclaration,
    /,
) -> list[SnapshotSource]:
    """Archive every declaration that produced the Run, in application order.

    The archived set matches the environment chain rather than the raw request,
    so an Overlay that normalized away does not appear to have been applied.
    """
    sources = [SnapshotSource(relative="base", root=base.root)]
    sources.extend(
        SnapshotSource(
            relative=f"overlays/{index:02d}-{overlay.identity.name}",
            root=overlay.root,
        )
        for index, overlay in enumerate(overlays)
    )
    sources.append(SnapshotSource(relative="task", root=task.root))
    return sources


def _create_arguments(accepted: _AcceptedRun) -> list[str]:
    storage = accepted.storage
    environment = accepted.spec.environment
    arguments = [
        "--interactive",
        "--init",
        "--name",
        f"runlab-{accepted.run_id.removeprefix('run:')}",
        "--label",
        "runlab.managed=true",
        "--label",
        f"runlab.run_id={accepted.run_id}",
        "--workdir",
        "/workspace",
        "--mount",
        f"type=bind,source={storage.workspace},target=/workspace",
        "--mount",
        f"type=bind,source={storage.artifacts},target=/artifacts",
    ]
    logs = accepted.request.base.definition.logs
    if storage.runtime_logs is not None and logs is not None:
        arguments.extend(
            ["--mount", f"type=bind,source={storage.runtime_logs},target={logs.target}"]
        )
    policy = accepted.request.policy
    if policy.memory is not None:
        arguments.extend(["--memory", policy.memory])
    if policy.cpus is not None:
        arguments.extend(["--cpus", str(policy.cpus)])
    if environment.network == "none":
        arguments.extend(["--network", "none"])
    for name, value in sorted(environment.env.items()):
        arguments.extend(["--env", f"{name}={value}"])
    for mount in accepted.mounts:
        arguments.extend(
            [
                "--mount",
                f"type=bind,source={mount.source},target={mount.target},readonly",
            ]
        )
    arguments.append(accepted.environment.image)
    return arguments


def _collect(accepted: _AcceptedRun, outcome: RunOutcome) -> CollectionSnapshot:
    try:
        return collect_run_storage(
            accepted.storage,
            require_artifacts=outcome is RunOutcome.SUCCEEDED,
            require_runtime_logs=(
                outcome is RunOutcome.SUCCEEDED
                and accepted.request.base.definition.logs is not None
            ),
        )
    except OSError:
        message = "could not collect the retained Run files"
        return CollectionSnapshot(
            inputs=FileSet(root="inputs", error=message),
            artifacts=FileSet(root="artifacts", error=message),
            logs=Logs(
                runtime=(
                    "logs/runtime"
                    if accepted.storage.runtime_logs is not None
                    else None
                ),
                error=message,
            ),
            workspace_bytes=0,
            artifact_bytes=0,
            log_bytes=0,
        )


def _build_record(
    accepted: _AcceptedRun, facts: _ExecutionFacts, collection: CollectionSnapshot
) -> RunRecord:
    completed_at = datetime.now(UTC)
    container = facts.container
    if container.started_at is not None:
        container = container.model_copy(update={"finished_at": completed_at})
    measurements = facts.measurements.model_copy(
        update={
            "workspace_bytes_at_completion": collection.workspace_bytes,
            "artifact_bytes": collection.artifact_bytes,
            "log_bytes": collection.log_bytes,
        }
    )
    return RunRecord(
        run_id=accepted.run_id,
        outcome=_collected_outcome(facts.outcome, collection),
        created_at=accepted.created_at,
        completed_at=completed_at,
        spec=accepted.spec,
        process=facts.process,
        container=container,
        measurements=measurements,
        inputs=collection.inputs,
        artifacts=collection.artifacts,
        logs=collection.logs,
    )


def _collected_outcome(
    outcome: RunOutcome, collection: CollectionSnapshot
) -> RunOutcome:
    if outcome is not RunOutcome.SUCCEEDED:
        return outcome
    if (
        collection.artifacts.error is not None
        or collection.logs.error is not None
        or collection.inputs.error is not None
    ):
        return RunOutcome.COLLECTION_FAILED
    return outcome


def _write_json(path: Path, payload: object, /) -> None:
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
