import json
from pathlib import Path

import pytest

from runlab.core.errors import DeclarationError
from runlab.declarations.loading import (
    effective_overlays,
    load_base,
    load_overlay,
    load_task,
)


def write_base(root: Path, definition: dict[str, object] | None = None) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    (root / "Dockerfile").write_text("FROM scratch\n")
    if definition is not None:
        (root / "base.json").write_text(json.dumps(definition))
    return root


def write_task(root: Path, instruction: str = "Write the answer.\n") -> Path:
    root.mkdir(parents=True, exist_ok=True)
    (root / "task.md").write_text(instruction)
    return root


def test_base_requires_a_dockerfile(tmp_path: Path) -> None:
    (tmp_path / "base").mkdir()

    with pytest.raises(DeclarationError, match="required file is missing"):
        load_base(tmp_path / "base")


def test_usage_aware_base_requires_native_logs(tmp_path: Path) -> None:
    write_base(tmp_path / "base", {"output_protocol": "pi-session-jsonl"})

    with pytest.raises(DeclarationError, match=r"logs\.target"):
        load_base(tmp_path / "base")


def test_base_digest_ignores_its_own_lock(tmp_path: Path) -> None:
    root = write_base(tmp_path / "base")
    before = load_base(root).identity.digest

    (root / "base.lock").write_text('{"declaration":"x","realizations":{}}')

    assert load_base(root).identity.digest == before


def test_task_rejects_a_dockerfile(tmp_path: Path) -> None:
    root = write_task(tmp_path / "task")
    (root / "Dockerfile").write_text("FROM scratch\n")

    with pytest.raises(DeclarationError, match="must not contain a Dockerfile"):
        load_task(root)


def test_overlay_reports_an_empty_declaration(tmp_path: Path) -> None:
    root = tmp_path / "overlay"
    root.mkdir()
    (root / "overlay.json").write_text("{}")

    assert load_overlay(root).definition.is_empty


def test_overlay_rejects_a_missing_mount_source(tmp_path: Path) -> None:
    root = tmp_path / "overlay"
    root.mkdir()
    (root / "overlay.json").write_text(
        json.dumps({"mounts": [{"source": "files", "target": "/opt/skills"}]})
    )

    with pytest.raises(DeclarationError, match="mount source is missing"):
        load_overlay(root)


def test_overlay_rejects_a_missing_layer_file(tmp_path: Path) -> None:
    root = tmp_path / "overlay"
    root.mkdir()
    (root / "overlay.json").write_text(json.dumps({"layer": "Dockerfile"}))

    with pytest.raises(DeclarationError, match="layer file is missing"):
        load_overlay(root)


def test_duplicate_credential_slots_are_rejected(tmp_path: Path) -> None:
    slot = {"name": "token", "kind": "file", "target": "/run/token"}
    write_base(tmp_path / "base", {"credentials": [slot, slot]})

    with pytest.raises(DeclarationError, match="unique"):
        load_base(tmp_path / "base")


def test_an_empty_overlay_normalizes_away(tmp_path: Path) -> None:
    empty = tmp_path / "baseline"
    empty.mkdir()
    (empty / "overlay.json").write_text("{}")
    configured = tmp_path / "offline"
    configured.mkdir()
    (configured / "overlay.json").write_text(json.dumps({"network": "none"}))

    kept = effective_overlays([load_overlay(empty), load_overlay(configured)])

    assert [item.identity.name for item in kept] == ["offline"]
