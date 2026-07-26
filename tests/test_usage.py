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
