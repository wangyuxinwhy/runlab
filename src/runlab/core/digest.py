"""Content addressing for declarations, stored files, and realization chains.

A digest is both the identity of a declaration and the address under which its
bytes can be retrieved later, so the algorithm has to stay stable across
releases: changing it invalidates every lock file and every stored Run.
"""

import hashlib
import uuid
from pathlib import Path
from typing import Protocol

from runlab.core.errors import DeclarationError

_CHUNK_SIZE = 1024 * 1024


class _Hasher(Protocol):
    def update(self, data: bytes, /) -> None: ...


def digest_file(path: Path, /) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(_CHUNK_SIZE):
            hasher.update(chunk)
    return f"sha256:{hasher.hexdigest()}"


def digest_directory(root: Path, /, *, exclude: frozenset[str] = frozenset()) -> str:
    """Digest a tree, skipping paths the caller declares outside its identity.

    `exclude` holds POSIX paths relative to `root`. Generated artifacts belong
    there: a lock file derived from a declaration cannot also contribute to the
    digest that lock file records.
    """
    resolved = root.resolve(strict=True)
    if not resolved.is_dir():
        message = f"not a directory: {resolved}"
        raise DeclarationError(message)
    hasher = hashlib.sha256()
    _digest_node(resolved, resolved, hasher, exclude)
    return f"sha256:{hasher.hexdigest()}"


def digest_values(*values: str) -> str:
    """Fold ordered strings into one digest.

    Order is significant: the realization chain it addresses is ordered, and
    two Overlays applied in the opposite sequence are a different environment.
    """
    hasher = hashlib.sha256()
    for value in values:
        hasher.update(value.encode())
        hasher.update(b"\0")
    return f"sha256:{hasher.hexdigest()}"


def new_id(prefix: str, /) -> str:
    return f"{prefix}:{uuid.uuid4().hex[:12]}"


def _digest_node(
    root: Path, path: Path, hasher: _Hasher, exclude: frozenset[str]
) -> None:
    relative_path = path.relative_to(root).as_posix()
    if relative_path in exclude:
        return
    relative = relative_path.encode()
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
            _digest_node(root, child, hasher, exclude)
        return
    message = f"unsupported filesystem entry: {path}"
    raise DeclarationError(message)
