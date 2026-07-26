from __future__ import annotations

import asyncio
import json
import logging
from datetime import UTC, datetime
from pathlib import Path

from runlab.errors import DefinitionError
from runlab.execution import Runner, RunRequest
from runlab.identity import new_id
from runlab.models import ExperimentRecord, ExperimentRun, RunPolicy
from runlab.packages import EnvironmentPackage, ExperimentPackage, TaskPackage

_LOGGER = logging.getLogger(__name__)


async def run_experiment(
    package: ExperimentPackage,
    output_root: Path,
    jobs: int,
    runner: Runner | None = None,
) -> tuple[ExperimentRecord, Path]:
    if jobs <= 0:
        msg = "jobs must be greater than zero"
        raise DefinitionError(msg)
    output_root = output_root.expanduser().resolve()
    output_root.mkdir(parents=True, exist_ok=True)
    runner = runner or Runner()
    experiment_id = new_id("experiment")
    created_at = datetime.now(UTC)
    semaphore = asyncio.Semaphore(jobs)
    policy = RunPolicy(
        timeout_seconds=package.definition.timeout_seconds,
        memory=package.definition.memory,
        cpus=package.definition.cpus,
        network=package.definition.network,
    )
    _LOGGER.info(
        "experiment %s environments=%d tasks=%d runs=%d jobs=%d",
        package.definition.name,
        len(package.environments),
        len(package.tasks),
        len(package.environments) * len(package.tasks),
        jobs,
    )
    runs = await _execute_matrix(
        package,
        output_root,
        policy,
        runner,
        semaphore,
    )
    completed_at = datetime.now(UTC)
    record = ExperimentRecord(
        experiment_id=experiment_id,
        name=package.definition.name,
        created_at=created_at,
        completed_at=completed_at,
        jobs=min(jobs, len(runs)),
        runs=runs,
    )
    record_path = output_root / f"{experiment_id.replace(':', '-')}.json"
    serialized = record.model_dump(mode="json")
    record_path.write_text(json.dumps(serialized, indent=2, sort_keys=True) + "\n")
    return record, record_path


async def _execute_matrix(
    package: ExperimentPackage,
    output_root: Path,
    policy: RunPolicy,
    runner: Runner,
    semaphore: asyncio.Semaphore,
) -> list[ExperimentRun]:
    async def execute(
        environment: EnvironmentPackage, task: TaskPackage
    ) -> ExperimentRun:
        async with semaphore:
            record, record_path = await runner.run(
                RunRequest(
                    environment=environment,
                    task=task,
                    output_root=output_root,
                    policy=policy,
                )
            )
        return ExperimentRun(
            environment=environment.identity.name,
            task=task.identity.name,
            run_id=record.run_id,
            outcome=record.outcome,
            record=str(record_path.relative_to(output_root)),
        )

    async with asyncio.TaskGroup() as group:
        pending_runs = [
            group.create_task(
                execute(environment, task),
                name=f"{environment.identity.name} x {task.identity.name}",
            )
            for environment in package.environments
            for task in package.tasks
        ]
    runs = [pending.result() for pending in pending_runs]
    runs.sort(key=lambda item: (item.environment, item.task))
    return runs
