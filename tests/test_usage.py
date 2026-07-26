import json
from pathlib import Path

from runlab.usage import collect_model_usage


def test_codex_usage_is_collected(tmp_path: Path) -> None:
    log = tmp_path / "stdout.log"
    log.write_text(
        "\n".join(
            [
                json.dumps({"type": "turn.started"}),
                json.dumps(
                    {
                        "type": "turn.completed",
                        "usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": 80,
                            "cache_write_input_tokens": 0,
                            "output_tokens": 20,
                            "reasoning_output_tokens": 5,
                        },
                    }
                ),
            ]
        )
    )

    usage, error = collect_model_usage(log, "codex-jsonl")

    assert error is None
    assert usage is not None
    assert usage.input_tokens == 100
    assert usage.output_tokens == 20


def test_missing_codex_usage_is_explicit(tmp_path: Path) -> None:
    log = tmp_path / "stdout.log"
    log.write_text('{"type":"turn.started"}\n')

    usage, error = collect_model_usage(log, "codex-jsonl")

    assert usage is None
    assert error == "codex-jsonl did not contain terminal usage"


def test_claude_usage_sums_every_reported_model(tmp_path: Path) -> None:
    log = tmp_path / "stdout.log"
    log.write_text(
        "\n".join(
            [
                json.dumps({"type": "system", "subtype": "init"}),
                json.dumps(
                    {
                        "type": "result",
                        "modelUsage": {
                            "claude-fable-5": {
                                "inputTokens": 10,
                                "cacheReadInputTokens": 100,
                                "cacheCreationInputTokens": 20,
                                "outputTokens": 30,
                                "costUSD": 1.25,
                            },
                            "claude-haiku-4-5": {
                                "inputTokens": 2,
                                "cacheReadInputTokens": 0,
                                "cacheCreationInputTokens": 0,
                                "outputTokens": 3,
                                "costUSD": 0.01,
                            },
                        },
                    }
                ),
            ]
        )
    )

    usage, error = collect_model_usage(log, "claude-stream-json")

    assert error is None
    assert usage is not None
    assert usage.input_tokens == 12
    assert usage.cached_input_tokens == 100
    assert usage.cache_write_input_tokens == 20
    assert usage.output_tokens == 33
    assert usage.reasoning_output_tokens == 0


def test_missing_claude_usage_is_explicit(tmp_path: Path) -> None:
    log = tmp_path / "stdout.log"
    log.write_text('{"type":"system","subtype":"init"}\n')

    usage, error = collect_model_usage(log, "claude-stream-json")

    assert usage is None
    assert error == "claude-stream-json did not contain terminal usage"
