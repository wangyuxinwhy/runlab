"""Public models are the source for validation, records, and generated Schema."""

from __future__ import annotations

from datetime import datetime
from enum import StrEnum
from typing import Annotated, Literal, cast

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

type Digest = Annotated[str, Field(pattern=r"^sha256:[0-9a-f]{64}$")]
type AbsoluteContainerPath = Annotated[str, Field(pattern=r"^/")]
type PositiveSeconds = Annotated[int, Field(gt=0)]
type OutputProtocol = Literal[
    "opaque",
    "codex-jsonl",
    "claude-stream-json",
    "pi-session-jsonl",
]


class ProtocolModel(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True, strict=True)


class CredentialRequest(ProtocolModel):
    """Declare one opaque credential slot consumed by an Environment."""

    name: Annotated[str, Field(pattern=r"^[a-z][a-z0-9_-]*$")]
    kind: Literal["file", "directory"]
    target: AbsoluteContainerPath


class InputRequest(ProtocolModel):
    name: Annotated[str, Field(pattern=r"^[a-z][a-z0-9_-]*$")]
    source_env: Annotated[str, Field(pattern=r"^[A-Z_][A-Z0-9_]*$")]
    target: AbsoluteContainerPath


class BuildInputRequest(ProtocolModel):
    name: Annotated[str, Field(pattern=r"^[a-z][a-z0-9_-]*$")]
    source_env: Annotated[str, Field(pattern=r"^[A-Z_][A-Z0-9_]*$")]
    include: tuple[str, ...] = ()

    @field_validator("include", mode="before")
    @classmethod
    def _accept_json_array(cls, value: object) -> object:
        if isinstance(value, list):
            return tuple(cast("list[object]", value))
        return value


class RuntimeLogs(ProtocolModel):
    """Declare where an Agent runtime writes its native audit log directory."""

    target: AbsoluteContainerPath


class EnvironmentDefinition(ProtocolModel):
    name: Annotated[str, Field(min_length=1)] | None = None
    output_protocol: OutputProtocol = "opaque"
    logs: RuntimeLogs | None = None
    credentials: list[CredentialRequest] = Field(default_factory=list)
    build_inputs: list[BuildInputRequest] = Field(default_factory=list)
    inputs: list[InputRequest] = Field(default_factory=list)
    metadata: dict[str, str] = Field(default_factory=dict)

    @model_validator(mode="after")
    def _validate_environment_contract(self) -> EnvironmentDefinition:
        if self.output_protocol != "opaque" and self.logs is None:
            msg = "usage-aware Environments must declare native logs"
            raise ValueError(msg)
        credential_names = [item.name for item in self.credentials]
        if len(credential_names) != len(set(credential_names)):
            msg = "Environment credential names must be unique"
            raise ValueError(msg)
        return self


class TaskDefinition(ProtocolModel):
    name: Annotated[str, Field(min_length=1)] | None = None
    inputs: list[InputRequest] = Field(default_factory=list)
    metadata: dict[str, str] = Field(default_factory=dict)


class ExperimentDefinition(ProtocolModel):
    name: Annotated[str, Field(min_length=1)]
    timeout_seconds: PositiveSeconds = 600
    memory: Annotated[str, Field(pattern=r"^[1-9][0-9]*(?:[bkmgBKMG])?$")] | None = None
    cpus: Annotated[float, Field(gt=0)] | None = None
    network: Literal["default", "none"] = "default"


class PackageIdentity(ProtocolModel):
    name: str
    digest: Digest


class ResolvedInput(ProtocolModel):
    name: str
    digest: Digest
    target: AbsoluteContainerPath
    kind: Literal["file", "directory"]


class ResolvedBuildInput(ProtocolModel):
    name: str
    digest: Digest
    kind: Literal["file", "directory"]


class ResolvedCredential(ProtocolModel):
    """Host credential paths and contents never enter the public record."""

    name: str
    kind: Literal["file", "directory"]
    target: AbsoluteContainerPath


class RunPolicy(ProtocolModel):
    timeout_seconds: PositiveSeconds
    memory: str | None = None
    cpus: float | None = None
    network: Literal["default", "none"] = "default"


class RunSpec(ProtocolModel):
    environment: PackageIdentity
    task: PackageIdentity
    policy: RunPolicy
    build_inputs: list[ResolvedBuildInput] = Field(default_factory=list)
    inputs: list[ResolvedInput] = Field(default_factory=list)
    credentials: list[ResolvedCredential] = Field(default_factory=list)


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
    image_digest: str | None = None
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
    """Missing values remain explicit when the engine cannot observe a metric."""

    wall_seconds: float | None = None
    peak_cpu_percent: float | None = None
    peak_memory_bytes: int | None = None
    peak_pids: int | None = None
    network_rx_bytes: int | None = None
    network_tx_bytes: int | None = None
    block_read_bytes: int | None = None
    block_write_bytes: int | None = None
    workspace_bytes_at_completion: int | None = None
    artifact_bytes: int = 0
    log_bytes: int = 0
    samples: int = 0
    model_usage: ModelUsage | None = None
    model_usage_error: str | None = None


class StoredFile(ProtocolModel):
    """Describe one immutable file relative to its Run directory."""

    path: str
    size_bytes: Annotated[int, Field(ge=0)]
    digest: Digest


class Artifacts(ProtocolModel):
    """List the deliverables available for task-quality evaluation."""

    root: Literal["artifacts"] = "artifacts"
    files: list[StoredFile] = Field(default_factory=list)
    error: str | None = None


class Logs(ProtocolModel):
    """List audit evidence while keeping runtime sessions as one log role."""

    root: Literal["logs"] = "logs"
    stdout: Literal["logs/stdout.log"] = "logs/stdout.log"
    stderr: Literal["logs/stderr.log"] = "logs/stderr.log"
    measurements: Literal["logs/measurements.jsonl"] = "logs/measurements.jsonl"
    runtime: Literal["logs/runtime"] | None = None
    files: list[StoredFile] = Field(default_factory=list)
    error: str | None = None


class RunRecord(ProtocolModel):
    schema_version: Literal[3] = 3
    run_id: str
    outcome: RunOutcome
    created_at: datetime
    completed_at: datetime
    spec: RunSpec
    process: ProcessResult
    container: ContainerResult
    measurements: Measurements
    artifacts: Artifacts = Field(default_factory=Artifacts)
    logs: Logs = Field(default_factory=Logs)


class ExperimentRun(ProtocolModel):
    environment: str
    task: str
    run_id: str
    outcome: RunOutcome
    record: str


class ExperimentRecord(ProtocolModel):
    schema_version: Literal[1] = 1
    experiment_id: str
    name: str
    created_at: datetime
    completed_at: datetime
    jobs: int
    runs: list[ExperimentRun]
