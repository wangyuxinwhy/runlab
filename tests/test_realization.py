from pathlib import Path

from runlab.core.models import LockFile
from runlab.realization.chain import environment_digest
from runlab.realization.locking import (
    locked_realization,
    read_lock,
    record_realization,
    write_lock,
)

DECLARATION = "sha256:" + "1" * 64
OLD_DECLARATION = "sha256:" + "2" * 64
NEW_DECLARATION = "sha256:" + "3" * 64
IMAGE_A = "sha256:" + "a" * 64
IMAGE_B = "sha256:" + "b" * 64
BASE_IMAGE = "sha256:" + "c" * 64


def test_recording_a_realization_preserves_existing_entries() -> None:
    first = record_realization(None, DECLARATION, "linux/arm64", IMAGE_A)
    second = record_realization(first, DECLARATION, "linux/amd64", IMAGE_B)

    assert second.realizations == {
        "linux/arm64": IMAGE_A,
        "linux/amd64": IMAGE_B,
    }


def test_a_changed_declaration_starts_a_fresh_entry_set() -> None:
    original = record_realization(None, OLD_DECLARATION, "linux/arm64", IMAGE_A)

    updated = record_realization(original, NEW_DECLARATION, "linux/arm64", IMAGE_B)

    assert updated.realizations == {"linux/arm64": IMAGE_B}


def test_a_lock_for_another_declaration_resolves_to_nothing() -> None:
    lock = LockFile(declaration=OLD_DECLARATION, realizations={"key": IMAGE_A})

    assert locked_realization(lock, NEW_DECLARATION, "key") is None


def test_a_lock_round_trips_through_disk(tmp_path: Path) -> None:
    path = tmp_path / "base.lock"
    lock = LockFile(declaration=DECLARATION, realizations={"key": IMAGE_A})

    write_lock(path, lock)

    assert read_lock(path) == lock


def test_a_missing_lock_reads_as_absent(tmp_path: Path) -> None:
    assert read_lock(tmp_path / "absent.lock") is None


def test_overlay_order_changes_the_environment() -> None:
    forward = environment_digest(
        BASE_IMAGE, overlay_realizations=["a", "b"], env={}, network="default"
    )
    reversed_order = environment_digest(
        BASE_IMAGE, overlay_realizations=["b", "a"], env={}, network="default"
    )

    assert forward != reversed_order


def test_capability_and_env_participate_in_the_environment() -> None:
    plain = environment_digest(
        BASE_IMAGE, overlay_realizations=[], env={}, network="default"
    )
    offline = environment_digest(
        BASE_IMAGE, overlay_realizations=[], env={}, network="none"
    )
    configured = environment_digest(
        BASE_IMAGE, overlay_realizations=[], env={"MODEL": "x"}, network="default"
    )

    assert len({plain, offline, configured}) == 3
