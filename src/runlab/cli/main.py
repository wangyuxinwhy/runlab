"""The agent-facing command surface.

Every command prints one compact JSON object on stdout so a caller can parse a
result without reading decorative text; progress and diagnostics go to stderr.
"""

import asyncio
import json
import logging
from importlib.metadata import version
from pathlib import Path

import click
from pydantic import BaseModel

from runlab.cli.documentation import list_topics, read_topic
from runlab.core.errors import RunLabError
from runlab.core.models import (
    BaseDefinition,
    LockFile,
    OverlayDefinition,
    RunOutcome,
    RunPolicy,
    RunRecord,
    TaskDefinition,
)
from runlab.declarations.loading import load_base, load_overlay, load_task
from runlab.engine.building import build_environment
from runlab.engine.execution import Runner, RunRequest

_SCHEMAS: dict[str, type[BaseModel]] = {
    "base": BaseDefinition,
    "overlay": OverlayDefinition,
    "task": TaskDefinition,
    "lock": LockFile,
    "run-record": RunRecord,
}

_DIRECTORY = click.Path(path_type=Path, file_okay=False)


class NaturalOrderGroup(click.Group):
    def list_commands(self, ctx: click.Context) -> list[str]:
        del ctx
        return list(self.commands)


@click.group(
    cls=NaturalOrderGroup,
    context_settings={"help_option_names": ["-h", "--help"], "max_content_width": 100},
)
@click.version_option(version("runlab"), prog_name="runlab")
def cli() -> None:
    """Record reproducible Agent execution facts.

    The fixed operation is Base + Overlay + Task -> Run. Composition of Runs
    into experiments belongs to the caller.
    """


@cli.group(cls=NaturalOrderGroup)
def base() -> None:
    """Work with Agent runtime Bases."""


@base.command("check")
@click.argument("directory", type=_DIRECTORY)
def base_check(directory: Path) -> None:
    """Validate a Base DIRECTORY and print its declaration identity."""
    declaration = load_base(directory)
    definition = declaration.definition
    _emit(
        {
            "name": declaration.identity.name,
            "declaration": declaration.identity.digest,
            "output_protocol": definition.output_protocol,
            "logs": None if definition.logs is None else definition.logs.target,
            "credentials": [item.name for item in definition.credentials],
            "inputs": [item.name for item in definition.inputs],
            "locked": declaration.lock_path.is_file(),
        }
    )


@base.command("build")
@click.argument("directory", type=_DIRECTORY)
@click.option("--rebuild", is_flag=True, help="Accept losing an unavailable baseline.")
def base_build(directory: Path, rebuild: bool) -> None:
    """Fix a Base DIRECTORY into a realization and write its lock."""
    declaration = load_base(directory)
    result = asyncio.run(build_environment(declaration, overlays=[], rebuild=rebuild))
    _emit(
        {
            "name": declaration.identity.name,
            "declaration": declaration.identity.digest,
            "platform": result.platform,
            "realization": result.realization,
            "lock": str(declaration.lock_path),
        }
    )


@cli.group(cls=NaturalOrderGroup)
def overlay() -> None:
    """Work with capability Overlays."""


@overlay.command("check")
@click.argument("directory", type=_DIRECTORY)
def overlay_check(directory: Path) -> None:
    """Validate an Overlay DIRECTORY and print its declaration identity."""
    declaration = load_overlay(directory)
    definition = declaration.definition
    _emit(
        {
            "name": declaration.identity.name,
            "declaration": declaration.identity.digest,
            "empty": definition.is_empty,
            "layer": definition.layer,
            "mounts": [item.target for item in definition.mounts],
            "env": sorted(definition.env),
            "network": definition.network,
            "credentials": [item.name for item in definition.credentials],
        }
    )


@overlay.command("build")
@click.argument("directory", type=_DIRECTORY)
@click.option("--base", "base_directory", required=True, type=_DIRECTORY)
@click.option("--rebuild", is_flag=True, help="Accept losing an unavailable baseline.")
def overlay_build(directory: Path, base_directory: Path, rebuild: bool) -> None:
    """Fix an Overlay DIRECTORY on top of a Base and write its lock."""
    declaration = load_overlay(directory)
    result = asyncio.run(
        build_environment(
            load_base(base_directory), overlays=[declaration], rebuild=rebuild
        )
    )
    _emit(
        {
            "name": declaration.identity.name,
            "declaration": declaration.identity.digest,
            "realization": result.realization,
            "environment": result.environment,
            "lock": str(declaration.lock_path),
        }
    )


@cli.group(cls=NaturalOrderGroup)
def task() -> None:
    """Work with Agent Tasks."""


@task.command("check")
@click.argument("directory", type=_DIRECTORY)
def task_check(directory: Path) -> None:
    """Validate a Task DIRECTORY and print its declaration identity."""
    declaration = load_task(directory)
    _emit(
        {
            "name": declaration.identity.name,
            "declaration": declaration.identity.digest,
            "instruction": declaration.instruction_digest,
            "workspace": declaration.workspace is not None,
            "inputs": [item.name for item in declaration.definition.inputs],
        }
    )


@cli.group(cls=NaturalOrderGroup)
def run() -> None:
    """Execute one Base + Overlay + Task Run."""


@run.command("start")
@click.option("--base", "base_directory", required=True, type=_DIRECTORY)
@click.option(
    "--overlay",
    "overlay_directories",
    multiple=True,
    type=_DIRECTORY,
    help="Capability Overlay; repeatable and order-sensitive.",
)
@click.option("--task", "task_directory", required=True, type=_DIRECTORY)
@click.option(
    "--output",
    type=_DIRECTORY,
    default=Path("runs"),
    show_default=True,
    help="Directory that receives immutable Run directories.",
)
@click.option(
    "--timeout-seconds", type=click.IntRange(min=1), default=600, show_default=True
)
@click.option("--memory", help="Docker memory limit, for example 4g.")
@click.option("--cpus", type=click.FloatRange(min=0, min_open=True))
@click.option(
    "--credentials",
    "credential_directory",
    type=_DIRECTORY,
    envvar="RUNLAB_CREDENTIALS",
    show_envvar=True,
    help="Private entries by declared name.",
)
@click.option("--rebuild", is_flag=True, help="Accept losing an unavailable baseline.")
def run_start(
    base_directory: Path,
    overlay_directories: tuple[Path, ...],
    task_directory: Path,
    output: Path,
    timeout_seconds: int,
    memory: str | None,
    cpus: float | None,
    credential_directory: Path | None,
    rebuild: bool,
) -> None:
    """Start one Run and print its terminal summary."""
    request = RunRequest(
        base=load_base(base_directory),
        overlays=[load_overlay(item) for item in overlay_directories],
        task=load_task(task_directory),
        output_root=output,
        policy=RunPolicy(timeout_seconds=timeout_seconds, memory=memory, cpus=cpus),
        credential_directory=credential_directory,
        rebuild=rebuild,
    )
    record, record_path = asyncio.run(Runner().run(request))
    _emit(
        {
            "run_id": record.run_id,
            "outcome": record.outcome,
            "environment": record.spec.environment.digest,
            "record": str(record_path),
        }
    )
    if record.outcome in {RunOutcome.SETUP_FAILED, RunOutcome.COLLECTION_FAILED}:
        raise click.exceptions.Exit(1)


@cli.group(cls=NaturalOrderGroup)
def docs() -> None:
    """Read the reference layer bundled with this CLI."""


@docs.command("list")
def docs_list() -> None:
    """List bundled reference topics."""
    _emit({"topics": list_topics()})


@docs.command("get")
@click.argument("topic")
def docs_get(topic: str) -> None:
    """Print one bundled reference TOPIC."""
    _emit({"topic": topic, "content": read_topic(topic)})


@cli.group(cls=NaturalOrderGroup)
def schema() -> None:
    """Inspect JSON Schemas generated from the public models."""


@schema.command("list")
def schema_list() -> None:
    """List public schema names."""
    _emit({"schemas": list(_SCHEMAS)})


@schema.command("show")
@click.argument("name", type=click.Choice(list(_SCHEMAS)))
def schema_show(name: str) -> None:
    """Print one generated JSON Schema."""
    click.echo(json.dumps(_SCHEMAS[name].model_json_schema(), indent=2, sort_keys=True))


def _emit(value: object, /) -> None:
    click.echo(json.dumps(value, separators=(",", ":"), default=_json_default))


def _json_default(value: object) -> object:
    if isinstance(value, BaseModel):
        return value.model_dump(mode="json")
    if isinstance(value, RunOutcome):
        return value.value
    message = f"cannot encode {type(value).__name__}"
    raise TypeError(message)


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
