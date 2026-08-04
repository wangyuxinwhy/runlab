import json
from pathlib import Path

from runlab.usage.collection import collect_model_usage


def test_codex_usage_tolerates_fields_the_runtime_adds(tmp_path: Path) -> None:
    log = tmp_path / "stdout.log"
    log.write_text(
        "\n".join(
            [
                "starting codex",
                json.dumps({"type": "turn.started"}),
                json.dumps(
                    {
                        "type": "turn.completed",
                        "usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": 80,
                            "output_tokens": 20,
                            "reasoning_output_tokens": 5,
                            "total_cost_usd": 0.01,
                        },
                    }
                ),
            ]
        )
    )

    usage, error = collect_model_usage(
        "codex-jsonl", stdout_path=log, runtime_logs_path=None
    )

    assert error is None
    assert usage is not None
    assert usage.input_tokens == 100
    assert usage.cache_write_input_tokens == 0


def test_claude_usage_sums_every_reported_model(tmp_path: Path) -> None:
    log = tmp_path / "stdout.log"
    log.write_text(
        json.dumps(
            {
                "type": "result",
                "modelUsage": {
                    "a": {
                        "inputTokens": 10,
                        "cacheReadInputTokens": 100,
                        "cacheCreationInputTokens": 20,
                        "outputTokens": 30,
                    },
                    "b": {"inputTokens": 2, "outputTokens": 3},
                },
            }
        )
    )

    usage, error = collect_model_usage(
        "claude-stream-json", stdout_path=log, runtime_logs_path=None
    )

    assert error is None
    assert usage is not None
    assert usage.input_tokens == 12
    assert usage.output_tokens == 33


def test_pi_usage_sums_assistant_messages(tmp_path: Path) -> None:
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    (sessions / "one.jsonl").write_text(
        "\n".join(
            [
                json.dumps({"type": "session"}),
                json.dumps(
                    {
                        "type": "message",
                        "message": {
                            "role": "assistant",
                            "usage": {
                                "input": 7,
                                "cacheRead": 1,
                                "cacheWrite": 2,
                                "output": 4,
                                "reasoning": 3,
                            },
                        },
                    }
                ),
                json.dumps({"type": "message", "message": {"role": "user"}}),
            ]
        )
    )

    usage, error = collect_model_usage(
        "pi-session-jsonl",
        stdout_path=tmp_path / "absent.log",
        runtime_logs_path=sessions,
    )

    assert error is None
    assert usage is not None
    assert usage.input_tokens == 7
    assert usage.reasoning_output_tokens == 3


def test_missing_usage_is_reported_rather_than_assumed(tmp_path: Path) -> None:
    log = tmp_path / "stdout.log"
    log.write_text('{"type":"turn.started"}\n')

    usage, error = collect_model_usage(
        "codex-jsonl", stdout_path=log, runtime_logs_path=None
    )

    assert usage is None
    assert error == "codex-jsonl contained no terminal usage"


def test_an_opaque_protocol_derives_nothing(tmp_path: Path) -> None:
    usage, error = collect_model_usage(
        "opaque", stdout_path=tmp_path / "absent.log", runtime_logs_path=None
    )

    assert usage is None
    assert error is None
