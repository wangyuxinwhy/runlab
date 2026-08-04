"""Deriving normalized model usage from what an Agent runtime actually emitted.

Each runtime reports tokens in its own shape, so this package owns those
shapes and nothing else owns them. Every adapter is tolerant of unknown fields:
a runtime adding a field to its own output is not a RunLab protocol violation,
and treating it as one would discard a core asset over a cosmetic change.

Normalization stays reversible. Raw stdout and the native session directory are
retained beside the record, so a derived count can always be recomputed.
"""

import json
from pathlib import Path
from typing import Annotated, cast

from pydantic import BaseModel, ConfigDict, Field, ValidationError

from runlab.core.models import ModelUsage, OutputProtocol


class _TolerantModel(BaseModel):
    model_config = ConfigDict(extra="ignore")


class _CodexUsage(_TolerantModel):
    input_tokens: int = 0
    cached_input_tokens: int = 0
    cache_write_input_tokens: int = 0
    output_tokens: int = 0
    reasoning_output_tokens: int = 0


class _ClaudeModelUsage(_TolerantModel):
    input_tokens: Annotated[int, Field(alias="inputTokens")] = 0
    cached_input_tokens: Annotated[int, Field(alias="cacheReadInputTokens")] = 0
    cache_write_input_tokens: Annotated[
        int, Field(alias="cacheCreationInputTokens")
    ] = 0
    output_tokens: Annotated[int, Field(alias="outputTokens")] = 0


class _ClaudeResult(_TolerantModel):
    model_usage: Annotated[
        dict[str, _ClaudeModelUsage], Field(alias="modelUsage", min_length=1)
    ]


class _PiUsage(_TolerantModel):
    input_tokens: Annotated[int, Field(alias="input")] = 0
    cached_input_tokens: Annotated[int, Field(alias="cacheRead")] = 0
    cache_write_input_tokens: Annotated[int, Field(alias="cacheWrite")] = 0
    output_tokens: Annotated[int, Field(alias="output")] = 0
    reasoning_output_tokens: Annotated[int, Field(alias="reasoning")] = 0


class _PiMessage(_TolerantModel):
    role: str = ""
    usage: _PiUsage | None = None


class _PiEvent(_TolerantModel):
    type: str = ""
    message: _PiMessage | None = None


def collect_model_usage(
    protocol: OutputProtocol,
    /,
    *,
    stdout_path: Path,
    runtime_logs_path: Path | None,
) -> tuple[ModelUsage | None, str | None]:
    if protocol == "opaque":
        return None, None
    if protocol == "codex-jsonl":
        return _collect_codex(stdout_path)
    if protocol == "claude-stream-json":
        return _collect_claude(stdout_path)
    return _collect_pi(runtime_logs_path)


def _collect_codex(path: Path) -> tuple[ModelUsage | None, str | None]:
    total = _empty()
    found = False
    try:
        with path.open() as stream:
            for line in stream:
                event = _decode(line)
                if event is None or event.get("type") != "turn.completed":
                    continue
                usage = _CodexUsage.model_validate(event.get("usage", {}))
                total = _add(
                    total,
                    ModelUsage(
                        input_tokens=usage.input_tokens,
                        cached_input_tokens=usage.cached_input_tokens,
                        cache_write_input_tokens=usage.cache_write_input_tokens,
                        output_tokens=usage.output_tokens,
                        reasoning_output_tokens=usage.reasoning_output_tokens,
                    ),
                )
                found = True
    except OSError, ValidationError:
        return None, "codex-jsonl usage could not be read"
    if not found:
        return None, "codex-jsonl contained no terminal usage"
    return total, None


def _collect_claude(path: Path) -> tuple[ModelUsage | None, str | None]:
    result: _ClaudeResult | None = None
    try:
        with path.open() as stream:
            for line in stream:
                event = _decode(line)
                if event is not None and event.get("type") == "result":
                    result = _ClaudeResult.model_validate(event)
    except OSError, ValidationError:
        return None, "claude-stream-json usage could not be read"
    if result is None:
        return None, "claude-stream-json contained no terminal usage"
    total = _empty()
    for model in result.model_usage.values():
        total = _add(
            total,
            ModelUsage(
                input_tokens=model.input_tokens,
                cached_input_tokens=model.cached_input_tokens,
                cache_write_input_tokens=model.cache_write_input_tokens,
                output_tokens=model.output_tokens,
                reasoning_output_tokens=0,
            ),
        )
    return total, None


def _collect_pi(path: Path | None) -> tuple[ModelUsage | None, str | None]:
    if path is None:
        return None, "pi-session-jsonl requires a declared native log directory"
    total = _empty()
    found = False
    try:
        for log in sorted(path.rglob("*.jsonl")):
            if log.is_symlink():
                continue
            with log.open() as stream:
                for line in stream:
                    increment = _pi_assistant_usage(line)
                    if increment is not None:
                        total = _add(total, increment)
                        found = True
    except OSError, ValidationError:
        return None, "pi-session-jsonl usage could not be read"
    if not found:
        return None, "pi-session-jsonl contained no assistant usage"
    return total, None


def _pi_assistant_usage(line: str) -> ModelUsage | None:
    payload = _decode(line)
    if payload is None:
        return None
    event = _PiEvent.model_validate(payload)
    message = event.message
    if event.type != "message" or message is None or message.role != "assistant":
        return None
    usage = message.usage
    if usage is None:
        return None
    return ModelUsage(
        input_tokens=usage.input_tokens,
        cached_input_tokens=usage.cached_input_tokens,
        cache_write_input_tokens=usage.cache_write_input_tokens,
        output_tokens=usage.output_tokens,
        reasoning_output_tokens=usage.reasoning_output_tokens,
    )


def _decode(line: str) -> dict[str, object] | None:
    """Skip lines that are not JSON objects.

    Runtimes interleave human-readable banners with their structured stream, so
    an undecodable line is normal output rather than evidence of corruption.
    """
    stripped = line.strip()
    if not stripped:
        return None
    try:
        payload: object = json.loads(stripped)
    except json.JSONDecodeError:
        return None
    if not isinstance(payload, dict):
        return None
    # JSON object keys are strings by the format's own definition.
    return cast("dict[str, object]", payload)


def _empty() -> ModelUsage:
    return ModelUsage(
        input_tokens=0,
        cached_input_tokens=0,
        cache_write_input_tokens=0,
        output_tokens=0,
        reasoning_output_tokens=0,
    )


def _add(left: ModelUsage, right: ModelUsage, /) -> ModelUsage:
    return ModelUsage(
        input_tokens=left.input_tokens + right.input_tokens,
        cached_input_tokens=left.cached_input_tokens + right.cached_input_tokens,
        cache_write_input_tokens=(
            left.cache_write_input_tokens + right.cache_write_input_tokens
        ),
        output_tokens=left.output_tokens + right.output_tokens,
        reasoning_output_tokens=(
            left.reasoning_output_tokens + right.reasoning_output_tokens
        ),
    )
