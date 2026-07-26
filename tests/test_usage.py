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

    usage, error = collect_model_usage(log, None, "codex-jsonl")

    assert error is None
    assert usage is not None
    assert usage.input_tokens == 100
    assert usage.output_tokens == 20


def test_missing_codex_usage_is_explicit(tmp_path: Path) -> None:
    log = tmp_path / "stdout.log"
    log.write_text('{"type":"turn.started"}\n')

    usage, error = collect_model_usage(log, None, "codex-jsonl")

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

    usage, error = collect_model_usage(log, None, "claude-stream-json")

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

    usage, error = collect_model_usage(log, None, "claude-stream-json")

    assert usage is None
    assert error == "claude-stream-json did not contain terminal usage"


def test_pi_usage_sums_native_assistant_messages(tmp_path: Path) -> None:
    stdout = tmp_path / "stdout.log"
    stdout.write_text("Only the final response.\n")
    runtime = tmp_path / "runtime"
    runtime.mkdir()
    log = runtime / "session.jsonl"
    log.write_text(
        "\n".join(
            [
                json.dumps({"type": "session", "id": "session-1"}),
                json.dumps(
                    {
                        "type": "message",
                        "id": "assistant-1",
                        "message": {
                            "role": "assistant",
                            "usage": {
                                "input": 100,
                                "output": 20,
                                "cacheRead": 80,
                                "cacheWrite": 10,
                                "reasoning": 5,
                                "totalTokens": 210,
                            },
                        },
                    }
                ),
                json.dumps(
                    {
                        "type": "message",
                        "id": "assistant-2",
                        "message": {
                            "role": "assistant",
                            "usage": {
                                "input": 40,
                                "output": 8,
                                "cacheRead": 160,
                                "cacheWrite": 0,
                                "totalTokens": 208,
                            },
                        },
                    }
                ),
            ]
        )
    )

    usage, error = collect_model_usage(stdout, runtime, "pi-session-jsonl")

    assert error is None
    assert usage is not None
    assert usage.input_tokens == 140
    assert usage.cached_input_tokens == 240
    assert usage.cache_write_input_tokens == 10
    assert usage.output_tokens == 28
    assert usage.reasoning_output_tokens == 5


def test_missing_pi_usage_is_explicit(tmp_path: Path) -> None:
    stdout = tmp_path / "stdout.log"
    stdout.touch()
    runtime = tmp_path / "runtime"
    runtime.mkdir()
    (runtime / "session.jsonl").write_text('{"type":"session"}\n')

    usage, error = collect_model_usage(stdout, runtime, "pi-session-jsonl")

    assert usage is None
    assert error == "pi-session-jsonl did not contain assistant usage"
