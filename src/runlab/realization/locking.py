"""Lock semantics: freezing a declaration into the realizations built from it.

This package owns what a lock means and how it accumulates. It does not know
how a realization is produced, so the container engine can change without
touching the rule that a recorded realization is never rewritten.
"""

import json
from pathlib import Path

from pydantic import ValidationError

from runlab.core.errors import RealizationError
from runlab.core.models import LockFile


def read_lock(path: Path, /) -> LockFile | None:
    if not path.is_file():
        return None
    try:
        return LockFile.model_validate(json.loads(path.read_text()))
    except (json.JSONDecodeError, ValidationError) as error:
        message = f"invalid lock file {path}: {error}"
        raise RealizationError(message) from error


def write_lock(path: Path, lock: LockFile, /) -> None:
    serialized = lock.model_dump(mode="json")
    path.write_text(json.dumps(serialized, indent=2, sort_keys=True) + "\n")


def locked_realization(
    lock: LockFile | None, declaration: str, key: str, /
) -> str | None:
    """Return the realization recorded for one key, if the lock still describes it.

    A lock whose declaration digest no longer matches describes a different
    source, so its entries say nothing about the declaration being resolved.
    """
    if lock is None or lock.declaration != declaration:
        return None
    return lock.realizations.get(key)


def record_realization(
    lock: LockFile | None, declaration: str, key: str, realization: str, /
) -> LockFile:
    """Add one entry, preserving every entry recorded for the same declaration.

    Entries accumulate because an existing entry is the reproducible baseline
    of every Run that referenced it. A declaration change starts a fresh set,
    since the old entries describe source that no longer exists here.
    """
    existing = (
        dict(lock.realizations)
        if lock is not None and lock.declaration == declaration
        else {}
    )
    existing[key] = realization
    return LockFile(declaration=declaration, realizations=existing)
