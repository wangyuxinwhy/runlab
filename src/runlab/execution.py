from __future__ import annotations

import asyncio
import json
import logging
import time
from collections.abc import Iterable
from contextlib import suppress
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from runlab.bindings import BindingResolver, HostBuildInput, HostMount
from runlab.docker import DockerEngine
from runlab.errors import ExecutionError, RunLabError
from runlab.identity import digest_values, new_id
from runlab.models import (
    Artifacts,
    ContainerResult,
    Logs,
    Measurements,
    OutputProtocol,
    ProcessResult,
    RunOutcome,
    RunPolicy,
    RunRecord,
    RunSpec,
)
from runlab.packages import EnvironmentPackage, TaskPackage
from runlab.stats import MeasurementAccumulator
from runlab.storage import (
    CollectionSnapshot,
    RunStorage,
    collect_run_storage,
    prepare_run_storage,
    remove_scratch,
    with_log_error,
)
from runlab.usage import collect_model_usage

_STATS_INTERVAL_SECONDS = 0.5
_LOGGER = logging.getLogger(__name__)


@dataclass(frozen=True, slots=True)
class RunRequest:
    environment: EnvironmentPackage
    task: TaskPackage
    output_root: Path
    policy: RunPolicy


@dataclass(frozen=True, slots=True)
class AcceptedRun:
    request: RunRequest
    run_id: str
    storage: RunStorage
    task_bytes: bytes
    spec: RunSpec
    input_mounts: list[HostMount]
    build_contexts: list[HostBuildInput]
    credential_mounts: list[HostMount]
    created_at: datetime


@dataclass(frozen=True, slots=True)
class ExecutionFacts:
    outcome: RunOutcome = RunOutcome.SETUP_FAILED
    process: ProcessResult = field(default_factory=ProcessResult)
    container: ContainerResult = field(default_factory=ContainerResult)
    measurements: Measurements = field(default_factory=Measurements)
    container_name: str | None = None


@dataclass(frozen=True, slots=True)
class ContainerProcess:
    name: str
    task_bytes: bytes
    stdout_path: Path
    stderr_path: Path
    measurements_path: Path
    timeout_seconds: int
    started_monotonic: float
    output_protocol: OutputProtocol


@dataclass(frozen=True, slots=True)
class ContainerSetup:
    name: str
    run_id: str
    image: str
    workspace: Path
    artifacts: Path
    runtime_logs: Path | None
    runtime_log_target: str | None
    input_mounts: list[HostMount]
    credential_mounts: list[HostMount]
    policy: RunPolicy


class Runner:
    def __init__(
        self,
        engine: DockerEngine | None = None,
        resolver: BindingResolver | None = None,
    ) -> None:
        self.engine = engine or DockerEngine()
        self.resolver = resolver or BindingResolver()

    def preflight_credentials(
        self,
        environments: Iterable[EnvironmentPackage],
        /,
    ) -> None:
        self.resolver.preflight_credentials(environments)

    async def run(self, request: RunRequest) -> tuple[RunRecord, Path]:
        """Execute one Run and always preserve a record after acceptance."""

        accepted = self._accept(request)
        facts = await self._execute(accepted)
        if facts.container_name is not None:
            with suppress(ExecutionError):
                await self.engine.remove(facts.container_name)
        collection = _collect(accepted, facts.outcome)
        try:
            remove_scratch(accepted.storage)
        except OSError:
            collection = with_log_error(
                collection,
                "could not remove temporary workspace",
            )
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
        record = RunRecord(
            run_id=accepted.run_id,
            outcome=_collected_outcome(facts.outcome, collection),
            created_at=accepted.created_at,
            completed_at=completed_at,
            spec=accepted.spec,
            process=facts.process,
            container=container,
            measurements=measurements,
            artifacts=collection.artifacts,
            logs=collection.logs,
        )
        record_path = accepted.storage.run_directory / "run.json"
        _write_record(record_path, record)
        _LOGGER.info("finish %s outcome=%s", record.run_id, record.outcome)
        return record, record_path

    def _accept(self, request: RunRequest) -> AcceptedRun:
        output_root = request.output_root.expanduser().resolve()
        output_root.mkdir(parents=True, exist_ok=True)
        bindings = self.resolver.resolve(
            request.environment,
            request.task,
        )
        spec = RunSpec(
            environment=request.environment.identity,
            task=request.task.identity,
            policy=request.policy,
            build_inputs=bindings.build_inputs,
            inputs=bindings.inputs,
            credentials=bindings.credentials,
        )
        run_id = new_id("run")
        storage = prepare_run_storage(
            output_root,
            request.task.root,
            run_id,
            collect_runtime_logs=request.environment.definition.logs is not None,
        )
        task_bytes = (request.task.root / "task.md").read_bytes()
        return AcceptedRun(
            request=request,
            run_id=run_id,
            storage=storage,
            task_bytes=task_bytes,
            spec=spec,
            input_mounts=bindings.input_mounts,
            build_contexts=bindings.build_contexts,
            credential_mounts=bindings.credential_mounts,
            created_at=datetime.now(UTC),
        )

    async def _execute(self, accepted: AcceptedRun) -> ExecutionFacts:
        environment = accepted.request.environment
        container_name: str | None = None
        started_monotonic: float | None = None
        container = ContainerResult()
        try:
            engine_version = await self.engine.check()
            _LOGGER.info(
                "prepare %s environment=%s",
                accepted.run_id,
                environment.identity.name,
            )
            image_identity = digest_values(
                environment.identity.digest,
                *(item.digest for item in accepted.spec.build_inputs),
            )
            image_tag = f"runlab-env:{image_identity.removeprefix('sha256:')[:16]}"
            build_contexts = {
                item.name: item.source for item in accepted.build_contexts
            }
            image_digest = await self.engine.ensure_image(
                environment.root,
                image_tag,
                build_contexts=build_contexts,
            )
            container_name = f"runlab-{accepted.run_id.removeprefix('run:')}"
            setup = ContainerSetup(
                name=container_name,
                run_id=accepted.run_id,
                image=image_tag,
                workspace=accepted.storage.workspace,
                artifacts=accepted.storage.artifacts,
                runtime_logs=accepted.storage.runtime_logs,
                runtime_log_target=(
                    None
                    if environment.definition.logs is None
                    else environment.definition.logs.target
                ),
                input_mounts=accepted.input_mounts,
                credential_mounts=accepted.credential_mounts,
                policy=accepted.request.policy,
            )
            container_id = await self.engine.create(_create_arguments(setup))
            _LOGGER.info(
                "start %s task=%s",
                accepted.run_id,
                accepted.request.task.identity.name,
            )
            started_at = datetime.now(UTC)
            started_monotonic = time.monotonic()
            container = ContainerResult(
                engine_version=engine_version,
                image_digest=image_digest,
                container_id=container_id,
                started_at=started_at,
            )
            process, measurements, outcome = await self._run_container(
                ContainerProcess(
                    name=container_name,
                    task_bytes=accepted.task_bytes,
                    stdout_path=accepted.storage.stdout,
                    stderr_path=accepted.storage.stderr,
                    measurements_path=accepted.storage.measurements,
                    timeout_seconds=accepted.request.policy.timeout_seconds,
                    started_monotonic=started_monotonic,
                    output_protocol=environment.definition.output_protocol,
                )
            )
            return ExecutionFacts(
                outcome=outcome,
                process=process,
                container=container,
                measurements=measurements,
                container_name=container_name,
            )
        except asyncio.CancelledError:
            if container_name is not None:
                with suppress(ExecutionError):
                    await self.engine.stop(container_name)
            wall_seconds = (
                None
                if started_monotonic is None
                else time.monotonic() - started_monotonic
            )
            return ExecutionFacts(
                outcome=RunOutcome.COLLECTION_FAILED,
                process=ProcessResult(error="run cancelled by operator"),
                container=container,
                measurements=Measurements(wall_seconds=wall_seconds),
                container_name=container_name,
            )
        except (RunLabError, OSError) as error:
            wall_seconds = (
                None
                if started_monotonic is None
                else time.monotonic() - started_monotonic
            )
            return ExecutionFacts(
                process=ProcessResult(error=str(error)),
                measurements=Measurements(wall_seconds=wall_seconds),
                container_name=container_name,
            )

    async def _run_container(
        self,
        request: ContainerProcess,
        /,
    ) -> tuple[ProcessResult, Measurements, RunOutcome]:
        accumulator = MeasurementAccumulator()
        stop_sampling = asyncio.Event()
        sampler = asyncio.create_task(
            self._sample(
                request.name,
                accumulator,
                request.measurements_path,
                stop_sampling,
            ),
            name=f"stats:{request.name}",
        )
        timed_out = False
        try:
            with (
                request.stdout_path.open("wb") as stdout,
                request.stderr_path.open("wb") as stderr,
            ):
                child = await asyncio.create_subprocess_exec(
                    self.engine.executable,
                    "start",
                    "--attach",
                    "--interactive",
                    request.name,
                    stdin=asyncio.subprocess.PIPE,
                    stdout=stdout,
                    stderr=stderr,
                )
                if child.stdin is None:
                    msg = "attached Docker client did not expose stdin"
                    raise ExecutionError(msg)
                child.stdin.write(request.task_bytes)
                await child.stdin.drain()
                child.stdin.close()
                try:
                    async with asyncio.timeout(request.timeout_seconds):
                        client_exit = await child.wait()
                except TimeoutError:
                    timed_out = True
                    await self.engine.stop(request.name)
                    client_exit = await child.wait()
        finally:
            stop_sampling.set()
            await sampler

        state = await self.engine.inspect_state(request.name)
        exit_code = int(state["ExitCode"])
        oom_killed = bool(state["OOMKilled"])
        docker_error = str(state.get("Error") or "") or None
        wall_seconds = time.monotonic() - request.started_monotonic
        measurements = accumulator.finish(wall_seconds)
        usage, usage_error = collect_model_usage(
            request.stdout_path,
            request.output_protocol,
        )
        measurements = measurements.model_copy(
            update={
                "model_usage": usage,
                "model_usage_error": usage_error,
            }
        )
        process = ProcessResult(
            exit_code=exit_code,
            oom_killed=oom_killed,
            error=docker_error,
        )
        if timed_out:
            outcome = RunOutcome.TIMED_OUT
        elif oom_killed:
            outcome = RunOutcome.OOM_KILLED
        elif exit_code == 0:
            outcome = RunOutcome.SUCCEEDED
        else:
            outcome = RunOutcome.AGENT_FAILED
        if client_exit != 0 and outcome is RunOutcome.SUCCEEDED:
            process = process.model_copy(
                update={"error": "attached Docker client failed"}
            )
            outcome = RunOutcome.COLLECTION_FAILED
        return process, measurements, outcome

    async def _sample(
        self,
        container_name: str,
        accumulator: MeasurementAccumulator,
        measurements_path: Path,
        stop: asyncio.Event,
    ) -> None:
        with measurements_path.open("a", encoding="utf-8", buffering=1) as stream:
            while not stop.is_set():
                try:
                    sample = await self.engine.stats(container_name)
                    accumulator.add(sample)
                    serialized = {
                        "captured_at": datetime.now(UTC).isoformat(),
                        "source": "docker_stats",
                        "stats": sample,
                    }
                    stream.write(json.dumps(serialized, sort_keys=True) + "\n")
                except ExecutionError, KeyError, TypeError, ValueError:
                    pass
                try:
                    await asyncio.wait_for(stop.wait(), timeout=_STATS_INTERVAL_SECONDS)
                except TimeoutError:
                    continue


def _create_arguments(setup: ContainerSetup, /) -> list[str]:
    arguments = [
        "--interactive",
        "--init",
        "--name",
        setup.name,
        "--label",
        "runlab.managed=true",
        "--label",
        f"runlab.run_id={setup.run_id}",
        "--workdir",
        "/workspace",
        "--mount",
        f"type=bind,source={setup.workspace},target=/workspace",
        "--mount",
        f"type=bind,source={setup.artifacts},target=/artifacts",
    ]
    if setup.runtime_logs is not None and setup.runtime_log_target is not None:
        arguments.extend(
            [
                "--mount",
                (
                    f"type=bind,source={setup.runtime_logs},"
                    f"target={setup.runtime_log_target}"
                ),
            ]
        )
    if setup.policy.memory is not None:
        arguments.extend(["--memory", setup.policy.memory])
    if setup.policy.cpus is not None:
        arguments.extend(["--cpus", str(setup.policy.cpus)])
    if setup.policy.network == "none":
        arguments.extend(["--network", "none"])
    for mount in [*setup.input_mounts, *setup.credential_mounts]:
        arguments.extend(
            [
                "--mount",
                f"type=bind,source={mount.source},target={mount.target},readonly",
            ]
        )
    arguments.append(setup.image)
    return arguments


def _collect(accepted: AcceptedRun, outcome: RunOutcome) -> CollectionSnapshot:
    try:
        return collect_run_storage(
            accepted.storage,
            require_artifacts=outcome is RunOutcome.SUCCEEDED,
            require_runtime_logs=(
                outcome is RunOutcome.SUCCEEDED
                and accepted.request.environment.definition.logs is not None
            ),
        )
    except OSError:
        message = "could not collect retained Run files"
        return CollectionSnapshot(
            artifacts=Artifacts(error=message),
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


def _collected_outcome(
    outcome: RunOutcome,
    collection: CollectionSnapshot,
) -> RunOutcome:
    if outcome is not RunOutcome.SUCCEEDED:
        return outcome
    if collection.artifacts.error is not None or collection.logs.error is not None:
        return RunOutcome.COLLECTION_FAILED
    return outcome


def _write_record(path: Path, model: object) -> None:
    serialized = model.model_dump(mode="json")  # type: ignore[attr-defined]
    path.write_text(json.dumps(serialized, indent=2, sort_keys=True) + "\n")
