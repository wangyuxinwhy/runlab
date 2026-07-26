from __future__ import annotations

import hashlib
import uuid
from pathlib import Path
from typing import Protocol

from runlab.errors import DefinitionError

_CHUNK_SIZE = 1024 * 1024


class _Hasher(Protocol):
    def update(self, data: bytes, /) -> None: ...


def digest_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(_CHUNK_SIZE):
            hasher.update(chunk)
    return f"sha256:{hasher.hexdigest()}"


def digest_directory(root: Path) -> str:
    root = root.resolve(strict=True)
    if not root.is_dir():
        msg = f"not a directory: {root}"
        raise DefinitionError(msg)
    hasher = hashlib.sha256()
    _digest_node(root, root, hasher)
    return f"sha256:{hasher.hexdigest()}"


def digest_values(*values: str) -> str:
    hasher = hashlib.sha256()
    for value in values:
        hasher.update(value.encode())
        hasher.update(b"\0")
    return f"sha256:{hasher.hexdigest()}"


def _digest_node(root: Path, path: Path, hasher: _Hasher) -> None:
    relative = path.relative_to(root).as_posix().encode()
    if path.is_symlink():
        hasher.update(b"symlink\0" + relative + b"\0")
        hasher.update(str(path.readlink()).encode())
        return
    if path.is_file():
        hasher.update(b"file\0" + relative + b"\0")
        with path.open("rb") as stream:
            while chunk := stream.read(_CHUNK_SIZE):
                hasher.update(chunk)
        return
    if path.is_dir():
        hasher.update(b"dir\0" + relative + b"\0")
        for child in sorted(path.iterdir(), key=lambda item: item.name):
            _digest_node(root, child, hasher)
        return
    msg = f"unsupported filesystem entry: {path}"
    raise DefinitionError(msg)


def new_id(prefix: str) -> str:
    return f"{prefix}:{uuid.uuid4().hex[:12]}"
