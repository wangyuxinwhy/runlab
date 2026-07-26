from __future__ import annotations

import json
from pathlib import Path
from typing import Literal

from pydantic import ValidationError

from runlab.models import ModelUsage


def collect_model_usage(
    path: Path,
    protocol: Literal["opaque", "codex-jsonl"],
    /,
) -> tuple[ModelUsage | None, str | None]:
    if protocol == "opaque":
        return None, None
    usage = ModelUsage(
        input_tokens=0,
        cached_input_tokens=0,
        cache_write_input_tokens=0,
        output_tokens=0,
        reasoning_output_tokens=0,
    )
    found = False
    try:
        with path.open() as stream:
            for line in stream:
                event = json.loads(line)
                if event.get("type") != "turn.completed":
                    continue
                item = ModelUsage.model_validate(event["usage"])
                usage = ModelUsage(
                    input_tokens=usage.input_tokens + item.input_tokens,
                    cached_input_tokens=(
                        usage.cached_input_tokens + item.cached_input_tokens
                    ),
                    cache_write_input_tokens=(
                        usage.cache_write_input_tokens + item.cache_write_input_tokens
                    ),
                    output_tokens=usage.output_tokens + item.output_tokens,
                    reasoning_output_tokens=(
                        usage.reasoning_output_tokens + item.reasoning_output_tokens
                    ),
                )
                found = True
    except OSError, json.JSONDecodeError, KeyError, ValidationError:
        return None, "codex-jsonl usage could not be parsed"
    if not found:
        return None, "codex-jsonl did not contain terminal usage"
    return usage, None
