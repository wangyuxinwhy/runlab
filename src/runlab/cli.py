from __future__ import annotations

import asyncio
import json
import logging
from pathlib import Path
from typing import Literal, cast

import click
from pydantic import BaseModel

from runlab import __version__
from runlab.bindings import BindingResolver
from runlab.errors import RunLabError
from runlab.execution import Runner, RunRequest
from runlab.experiment import run_experiment
from runlab.guidance import list_guidance, read_guidance
from runlab.models import (
    EnvironmentDefinition,
    ExperimentDefinition,
    ExperimentRecord,
    RunOutcome,
    RunPolicy,
    RunRecord,
    TaskDefinition,
)
from runlab.packages import load_environment, load_experiment, load_task

_SCHEMAS: dict[str, type[BaseModel]] = {
    "environment": EnvironmentDefinition,
    "task": TaskDefinition,
    "experiment": ExperimentDefinition,
    "run-record": RunRecord,
    "experiment-record": ExperimentRecord,
}


class NaturalOrderGroup(click.Group):
    def list_commands(self, ctx: click.Context) -> list[str]:
        del ctx
        return list(self.commands)


@click.group(
    cls=NaturalOrderGroup,
    context_settings={"help_option_names": ["-h", "--help"], "max_content_width": 100},
)
@click.version_option(__version__, prog_name="runlab")
def cli() -> None:
    """Run reproducible Agent experiments and preserve execution facts.

    The core operation is Environment + Task -> Run. Use EXPERIMENT RUN to execute a
    full Environment x Task matrix with bounded concurrency.
    """


@cli.group(cls=NaturalOrderGroup)
def environment() -> None:
    """Work with reusable Agent runtime Environments."""


@environment.command("check")
@click.argument("directory", type=click.Path(path_type=Path, file_okay=False))
def environment_check(directory: Path) -> None:
    """Validate an Environment DIRECTORY and print its identity."""

    package = load_environment(directory)
    _emit(
        {
            "name": package.identity.name,
            "digest": package.identity.digest,
            "output_protocol": package.definition.output_protocol,
            "logs": (
                None
                if package.definition.logs is None
                else package.definition.logs.model_dump(mode="json")
            ),
            "credentials": [
                item.model_dump(mode="json") for item in package.definition.credentials
            ],
            "build_inputs": [item.name for item in package.definition.build_inputs],
            "inputs": [item.name for item in package.definition.inputs],
        }
    )


@cli.group(cls=NaturalOrderGroup)
def task() -> None:
    """Work with Agent Tasks."""


@task.command("check")
@click.argument("directory", type=click.Path(path_type=Path, file_okay=False))
def task_check(directory: Path) -> None:
    """Validate a Task DIRECTORY and print its identity."""

    package = load_task(directory)
    _emit(
        {
            "name": package.identity.name,
            "digest": package.identity.digest,
            "instruction_digest": package.instruction_digest,
            "inputs": [item.name for item in package.definition.inputs],
        }
    )


@cli.group(cls=NaturalOrderGroup)
def run() -> None:
    """Execute one Environment + Task Run."""


@run.command("start")
@click.option(
    "--environment",
    "environment_directory",
    required=True,
    type=click.Path(path_type=Path, file_okay=False),
    help="Environment directory.",
)
@click.option(
    "--task",
    "task_directory",
    required=True,
    type=click.Path(path_type=Path, file_okay=False),
    help="Task directory.",
)
@click.option(
    "--output",
    type=click.Path(path_type=Path, file_okay=False),
    default=Path("runs"),
    show_default=True,
    help="Directory that receives immutable Run directories.",
)
@click.option(
    "--timeout-seconds", type=click.IntRange(min=1), default=600, show_default=True
)
@click.option("--memory", help="Docker memory limit, for example 4g.")
@click.option(
    "--cpus", type=click.FloatRange(min=0, min_open=True), help="Docker CPU limit."
)
@click.option(
    "--network",
    type=click.Choice(["default", "none"]),
    default="default",
    show_default=True,
)
@click.option(
    "--credentials",
    "credential_directory",
    type=click.Path(path_type=Path, file_okay=False),
    envvar="RUNLAB_CREDENTIALS",
    show_envvar=True,
    help=(
        "Private entries by declared name. Defaults to "
        "$XDG_CONFIG_HOME/runlab/credentials or ~/.config/runlab/credentials."
    ),
)
def run_start(
    **values: object,
) -> None:
    """Start one Run and print a compact terminal summary."""

    environment_directory = cast("Path", values["environment_directory"])
    task_directory = cast("Path", values["task_directory"])
    output = cast("Path", values["output"])
    credential_directory = cast("Path | None", values["credential_directory"])
    environment_package = load_environment(environment_directory)
    task_package = load_task(task_directory)
    policy = RunPolicy(
        timeout_seconds=cast("int", values["timeout_seconds"]),
        memory=cast("str | None", values["memory"]),
        cpus=cast("float | None", values["cpus"]),
        network=cast("Literal['default', 'none']", values["network"]),
    )
    record, record_path = asyncio.run(
        Runner(resolver=BindingResolver(credential_directory)).run(
            RunRequest(
                environment=environment_package,
                task=task_package,
                output_root=output,
                policy=policy,
            )
        )
    )
    _emit(
        {
            "run_id": record.run_id,
            "outcome": record.outcome,
            "record": str(record_path),
        }
    )
    if record.outcome in {RunOutcome.SETUP_FAILED, RunOutcome.COLLECTION_FAILED}:
        raise click.exceptions.Exit(1)


@cli.group(cls=NaturalOrderGroup)
def experiment() -> None:
    """Execute Environment x Task experiment plans."""


@experiment.command("check")
@click.argument("directory", type=click.Path(path_type=Path, file_okay=False))
def experiment_check(directory: Path) -> None:
    """Validate an Experiment DIRECTORY and print its execution-plan size."""

    package = load_experiment(directory)
    _emit(
        {
            "name": package.definition.name,
            "environments": len(package.environments),
            "tasks": len(package.tasks),
            "runs": len(package.environments) * len(package.tasks),
        }
    )


@experiment.command("run")
@click.argument("directory", type=click.Path(path_type=Path, file_okay=False))
@click.option(
    "--output",
    type=click.Path(path_type=Path, file_okay=False),
    default=Path("runs"),
    show_default=True,
)
@click.option("--jobs", type=click.IntRange(min=1), default=1, show_default=True)
@click.option(
    "--credentials",
    "credential_directory",
    type=click.Path(path_type=Path, file_okay=False),
    envvar="RUNLAB_CREDENTIALS",
    show_envvar=True,
    help=(
        "Private entries by declared name. Defaults to "
        "$XDG_CONFIG_HOME/runlab/credentials or ~/.config/runlab/credentials."
    ),
)
def experiment_run(
    directory: Path,
    output: Path,
    jobs: int,
    credential_directory: Path | None,
) -> None:
    """Execute the full matrix in Experiment DIRECTORY."""

    package = load_experiment(directory)
    runner = Runner(resolver=BindingResolver(credential_directory))
    record, record_path = asyncio.run(
        run_experiment(package, output, jobs, runner=runner)
    )
    counts = {
        outcome.value: sum(item.outcome == outcome for item in record.runs)
        for outcome in RunOutcome
        if any(item.outcome == outcome for item in record.runs)
    }
    _emit(
        {
            "experiment_id": record.experiment_id,
            "runs": len(record.runs),
            "outcomes": counts,
            "record": str(record_path),
        }
    )
    if any(
        item.outcome in {RunOutcome.SETUP_FAILED, RunOutcome.COLLECTION_FAILED}
        for item in record.runs
    ):
        raise click.exceptions.Exit(1)


@cli.group(cls=NaturalOrderGroup)
def guidance() -> None:
    """Read version-matched guidance distributed with RunLab."""


@guidance.command("list")
def guidance_list() -> None:
    """List embedded guidance documents."""

    _emit({"guidance": list_guidance()})


@guidance.command("show")
@click.argument("name", type=click.Choice(["environment"]))
def guidance_show(name: str) -> None:
    """Return one embedded guidance document."""

    _emit({"name": name, "content": read_guidance(name)})


@cli.group(cls=NaturalOrderGroup)
def schema() -> None:
    """Inspect JSON Schemas derived from RunLab's public models."""


@schema.command("list")
def schema_list() -> None:
    """List public Schema names."""

    _emit({"schemas": list(_SCHEMAS)})


@schema.command("show")
@click.argument("name", type=click.Choice(list(_SCHEMAS)))
def schema_show(name: str) -> None:
    """Print one generated JSON Schema."""

    click.echo(json.dumps(_SCHEMAS[name].model_json_schema(), indent=2, sort_keys=True))


def _emit(value: object) -> None:
    click.echo(json.dumps(value, separators=(",", ":"), default=_json_default))


def _json_default(value: object) -> object:
    if isinstance(value, BaseModel):
        return value.model_dump(mode="json")
    if isinstance(value, RunOutcome):
        return value.value
    msg = f"cannot encode {type(value).__name__}"
    raise TypeError(msg)


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    try:
        cli(standalone_mode=False)
    except RunLabError as error:
        click.echo(f"runlab: {error}", err=True)
        raise SystemExit(1) from error
    except click.ClickException as error:
        error.show()
        raise SystemExit(error.exit_code) from error
    except click.exceptions.Exit as error:
        raise SystemExit(error.exit_code) from error
    except click.exceptions.Abort as error:
        click.echo("Aborted!", err=True)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
