"""Protocol vocabulary exchanged across every package boundary.

These models are the source for validation, persisted records, and the JSON
Schema published by the CLI, so a field added here becomes a public contract.
"""

from collections.abc import Iterable
from datetime import datetime
from enum import StrEnum
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator

type Digest = Annotated[str, Field(pattern=r"^sha256:[0-9a-f]{64}$")]
type AbsoluteContainerPath = Annotated[str, Field(pattern=r"^/")]
type RelativePath = Annotated[str, Field(pattern=r"^[^/].*")]
type PositiveSeconds = Annotated[int, Field(gt=0)]
type SlugName = Annotated[str, Field(pattern=r"^[a-z][a-z0-9_-]*$")]
type EnvironmentVariableName = Annotated[str, Field(pattern=r"^[A-Z_][A-Z0-9_]*$")]
type OutputProtocol = Literal[
    "opaque",
    "codex-jsonl",
    "claude-stream-json",
    "pi-session-jsonl",
]
type NetworkMode = Literal["default", "none"]
type EntryKind = Literal["file", "directory"]


class ProtocolModel(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True, strict=True)


class CredentialRequest(ProtocolModel):
    """One opaque credential slot: a logical name, never a host path or value."""

    name: SlugName
    kind: EntryKind
    target: AbsoluteContainerPath


class InputRequest(ProtocolModel):
    """Read-only host material located through the caller's environment."""

    name: SlugName
    source_env: EnvironmentVariableName
    target: AbsoluteContainerPath


class BuildInputRequest(ProtocolModel):
    name: SlugName
    source_env: EnvironmentVariableName
    include: list[RelativePath] = []


class RuntimeLogs(ProtocolModel):
    """Where the Agent runtime writes its native session directory."""

    target: AbsoluteContainerPath


class BaseDefinition(ProtocolModel):
    """One Agent runtime and the execution contract its entrypoint honors."""

    name: str | None = None
    output_protocol: OutputProtocol = "opaque"
    logs: RuntimeLogs | None = None
    credentials: list[CredentialRequest] = []
    build_inputs: list[BuildInputRequest] = []
    inputs: list[InputRequest] = []
    metadata: dict[str, str] = {}

    @model_validator(mode="after")
    def _validate_usage_contract(self) -> BaseDefinition:
        if self.output_protocol != "opaque" and self.logs is None:
            message = "a usage-aware output_protocol requires logs.target"
            raise ValueError(message)
        _reject_duplicate_names(item.name for item in self.credentials)
        return self


class MountRequest(ProtocolModel):
    """A read-only tree supplied from inside the Overlay declaration."""

    source: RelativePath
    target: AbsoluteContainerPath


class OverlayDefinition(ProtocolModel):
    """Capability configuration applied on top of a Base.

    The four delivery forms are alternatives for the same concept, so an
    Overlay stays one experimental variable regardless of which it uses.
    """

    name: str | None = None
    layer: RelativePath | None = None
    mounts: list[MountRequest] = []
    env: dict[EnvironmentVariableName, str] = {}
    network: NetworkMode | None = None
    credentials: list[CredentialRequest] = []
    metadata: dict[str, str] = {}

    @model_validator(mode="after")
    def _validate_credential_names(self) -> OverlayDefinition:
        _reject_duplicate_names(item.name for item in self.credentials)
        return self

    @property
    def is_empty(self) -> bool:
        """An Overlay changing nothing normalizes away instead of forming a variant."""
        return not (
            self.layer or self.mounts or self.env or self.network or self.credentials
        )


class TaskDefinition(ProtocolModel):
    name: str | None = None
    inputs: list[InputRequest] = []
    metadata: dict[str, str] = {}


class DeclarationIdentity(ProtocolModel):
    name: str
    digest: Digest


class LockFile(ProtocolModel):
    """Freezes a declaration into the realizations built from it.

    Entries accumulate and are never rewritten: a removed entry takes the
    reproducible baseline of every Run that referenced it.
    """

    schema_version: Literal[1] = 1
    declaration: Digest
    realizations: dict[str, str] = {}


class RealizationKind(StrEnum):
    IMAGE = "image"
    CONTENT = "content"


class BaseBinding(ProtocolModel):
    name: str
    declaration: Digest
    realization: str
    platform: str | None = None


class OverlayBinding(ProtocolModel):
    name: str
    declaration: Digest
    realization: str
    kind: RealizationKind


class EnvironmentSpec(ProtocolModel):
    """What actually ran, folded into one comparability key.

    `digest` is derived from the ordered chain so that "did these Runs share
    an environment" is one comparison rather than a field-by-field walk.
    """

    digest: Digest
    base: BaseBinding
    overlays: list[OverlayBinding] = []
    env: dict[str, str] = {}
    network: NetworkMode = "default"


class RunPolicy(ProtocolModel):
    """Resource bounds and termination conditions, never capability."""

    timeout_seconds: PositiveSeconds
    memory: str | None = None
    cpus: float | None = None


class ResolvedCredential(ProtocolModel):
    """Host paths and contents never enter the public record."""

    name: str
    kind: EntryKind
    target: AbsoluteContainerPath


class ResolvedInput(ProtocolModel):
    name: str
    digest: Digest
    kind: EntryKind
    target: AbsoluteContainerPath


class ResolvedBuildInput(ProtocolModel):
    name: str
    digest: Digest
    kind: EntryKind


class RunSpec(ProtocolModel):
    environment: EnvironmentSpec
    task: DeclarationIdentity
    policy: RunPolicy
    credentials: list[ResolvedCredential] = []
    inputs: list[ResolvedInput] = []
    build_inputs: list[ResolvedBuildInput] = []


class RunOutcome(StrEnum):
    SUCCEEDED = "succeeded"
    AGENT_FAILED = "agent_failed"
    TIMED_OUT = "timed_out"
    OOM_KILLED = "oom_killed"
    SETUP_FAILED = "setup_failed"
    COLLECTION_FAILED = "collection_failed"


class ProcessResult(ProtocolModel):
    exit_code: int | None = None
    oom_killed: bool = False
    error: str | None = None


class ContainerResult(ProtocolModel):
    engine: Literal["docker"] = "docker"
    engine_version: str | None = None
    container_id: str | None = None
    started_at: datetime | None = None
    finished_at: datetime | None = None


class ModelUsage(ProtocolModel):
    input_tokens: int
    cached_input_tokens: int
    cache_write_input_tokens: int
    output_tokens: int
    reasoning_output_tokens: int


class Measurements(ProtocolModel):
    """A metric the engine could not observe stays null rather than guessed."""

    wall_seconds: float | None = None
    workspace_bytes_at_completion: int | None = None
    artifact_bytes: int = 0
    log_bytes: int = 0
    model_usage: ModelUsage | None = None
    model_usage_error: str | None = None


class StoredFile(ProtocolModel):
    """One immutable file addressed relative to its Run directory."""

    path: str
    size_bytes: Annotated[int, Field(ge=0)]
    digest: Digest


class FileSet(ProtocolModel):
    root: str
    files: list[StoredFile] = []
    error: str | None = None


class Logs(ProtocolModel):
    """Audit evidence, keeping every native runtime session as one log role."""

    root: Literal["logs"] = "logs"
    stdout: Literal["logs/stdout.log"] = "logs/stdout.log"
    stderr: Literal["logs/stderr.log"] = "logs/stderr.log"
    runtime: Literal["logs/runtime"] | None = None
    files: list[StoredFile] = []
    error: str | None = None


class RunRecord(ProtocolModel):
    schema_version: Literal[4] = 4
    run_id: str
    outcome: RunOutcome
    created_at: datetime
    completed_at: datetime
    spec: RunSpec
    process: ProcessResult
    container: ContainerResult
    measurements: Measurements
    inputs: FileSet
    artifacts: FileSet
    logs: Logs


def _reject_duplicate_names(names: Iterable[str]) -> None:
    collected = list(names)
    if len(collected) != len(set(collected)):
        message = "credential names must be unique within a declaration"
        raise ValueError(message)
