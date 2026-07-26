from __future__ import annotations

import json
from pathlib import Path
from typing import Annotated

from pydantic import BaseModel, ConfigDict, Field, ValidationError

from runlab.models import ModelUsage, OutputProtocol


class _ClaudeModelUsage(BaseModel):
    model_config = ConfigDict(extra="ignore", strict=True)

    input_tokens: Annotated[int, Field(alias="inputTokens")]
    cached_input_tokens: Annotated[int, Field(alias="cacheReadInputTokens")]
    cache_write_input_tokens: Annotated[int, Field(alias="cacheCreationInputTokens")]
    output_tokens: Annotated[int, Field(alias="outputTokens")]


class _ClaudeResult(BaseModel):
    model_config = ConfigDict(extra="ignore", strict=True)

    model_usage: Annotated[
        dict[str, _ClaudeModelUsage],
        Field(alias="modelUsage", min_length=1),
    ]


class _PiModelUsage(BaseModel):
    model_config = ConfigDict(extra="ignore", strict=True)

    input_tokens: Annotated[int, Field(alias="input")]
    cached_input_tokens: Annotated[int, Field(alias="cacheRead")]
    cache_write_input_tokens: Annotated[int, Field(alias="cacheWrite")]
    output_tokens: Annotated[int, Field(alias="output")]
    reasoning_output_tokens: Annotated[int, Field(alias="reasoning")] = 0


class _PiMessage(BaseModel):
    model_config = ConfigDict(extra="ignore", strict=True)

    role: str
    usage: _PiModelUsage | None = None


class _PiEvent(BaseModel):
    model_config = ConfigDict(extra="ignore", strict=True)

    type: str
    message: _PiMessage | None = None


def collect_model_usage(
    stdout_path: Path,
    runtime_logs_path: Path | None,
    protocol: OutputProtocol,
    /,
) -> tuple[ModelUsage | None, str | None]:
    if protocol == "opaque":
        return None, None
    if protocol == "codex-jsonl":
        return _collect_codex_usage(stdout_path)
    if protocol == "claude-stream-json":
        return _collect_claude_usage(stdout_path)
    return _collect_pi_usage(runtime_logs_path)


def _collect_codex_usage(path: Path) -> tuple[ModelUsage | None, str | None]:
    usage = _empty_usage()
    found = False
    try:
        with path.open() as stream:
            for line in stream:
                event = json.loads(line)
                if event.get("type") != "turn.completed":
                    continue
                usage = _add_usage(usage, ModelUsage.model_validate(event["usage"]))
                found = True
    except OSError, json.JSONDecodeError, KeyError, ValidationError:
        return None, "codex-jsonl usage could not be parsed"
    if not found:
        return None, "codex-jsonl did not contain terminal usage"
    return usage, None


def _collect_claude_usage(path: Path) -> tuple[ModelUsage | None, str | None]:
    result: _ClaudeResult | None = None
    try:
        with path.open() as stream:
            for line in stream:
                event = json.loads(line)
                if event.get("type") == "result":
                    result = _ClaudeResult.model_validate(event)
    except OSError, json.JSONDecodeError, AttributeError, ValidationError:
        return None, "claude-stream-json usage could not be parsed"
    if result is None:
        return None, "claude-stream-json did not contain terminal usage"
    usage = _empty_usage()
    for model in result.model_usage.values():
        usage = _add_usage(
            usage,
            ModelUsage(
                input_tokens=model.input_tokens,
                cached_input_tokens=model.cached_input_tokens,
                cache_write_input_tokens=model.cache_write_input_tokens,
                output_tokens=model.output_tokens,
                reasoning_output_tokens=0,
            ),
        )
    return usage, None


def _collect_pi_usage(path: Path | None) -> tuple[ModelUsage | None, str | None]:
    if path is None:
        return None, "pi-session-jsonl requires native logs"
    usage = _empty_usage()
    found = False
    try:
        for log in sorted(path.rglob("*.jsonl")):
            for increment in _read_pi_usage(log):
                usage = _add_usage(usage, increment)
                found = True
    except OSError, json.JSONDecodeError, ValueError, ValidationError:
        return None, "pi-session-jsonl usage could not be parsed"
    if not found:
        return None, "pi-session-jsonl did not contain assistant usage"
    return usage, None


def _read_pi_usage(path: Path, /) -> list[ModelUsage]:
    if path.is_symlink():
        msg = "Pi session log must be a regular file"
        raise ValueError(msg)
    usage: list[ModelUsage] = []
    with path.open() as stream:
        for line in stream:
            event = _PiEvent.model_validate_json(line)
            increment = _pi_assistant_usage(event)
            if increment is not None:
                usage.append(increment)
    return usage


def _pi_assistant_usage(event: _PiEvent, /) -> ModelUsage | None:
    message = event.message
    if event.type != "message" or message is None or message.role != "assistant":
        return None
    usage = message.usage
    if usage is None:
        msg = "Pi assistant message did not contain usage"
        raise ValueError(msg)
    return ModelUsage(
        input_tokens=usage.input_tokens,
        cached_input_tokens=usage.cached_input_tokens,
        cache_write_input_tokens=usage.cache_write_input_tokens,
        output_tokens=usage.output_tokens,
        reasoning_output_tokens=usage.reasoning_output_tokens,
    )


def _empty_usage() -> ModelUsage:
    return ModelUsage(
        input_tokens=0,
        cached_input_tokens=0,
        cache_write_input_tokens=0,
        output_tokens=0,
        reasoning_output_tokens=0,
    )


def _add_usage(left: ModelUsage, right: ModelUsage, /) -> ModelUsage:
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
