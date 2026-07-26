from __future__ import annotations

import asyncio
from collections import defaultdict
from pathlib import Path

import pytest

from runlab.docker import DockerEngine
from runlab.errors import ExecutionError


class ImageEngine(DockerEngine):
    def __init__(self) -> None:
        self._build_locks = defaultdict(asyncio.Lock)
        self.builds = 0
        self.built = False

    async def image_id(self, tag: str, *, required: bool) -> str | None:
        del tag, required
        return "sha256:image" if self.built else None

    async def run(self, *arguments: str, cwd: Path | None = None) -> str:
        del cwd
        if arguments[0] == "build":
            self.builds += 1
            await asyncio.sleep(0.01)
            self.built = True
        return ""


async def test_concurrent_image_requests_share_one_build(tmp_path: Path) -> None:
    engine = ImageEngine()

    results = await asyncio.gather(
        *(engine.ensure_image(tmp_path, "shared-image") for _ in range(3))
    )

    assert results == ["sha256:image"] * 3
    assert engine.builds == 1


class FailingCreateEngine(DockerEngine):
    def __init__(self) -> None:
        pass

    async def run(self, *arguments: str, cwd: Path | None = None) -> str:
        del arguments, cwd
        msg = "Docker operation failed: mount source /private/credential failed"
        raise ExecutionError(msg)


async def test_create_error_redacts_private_mount_sources() -> None:
    engine = FailingCreateEngine()

    with pytest.raises(ExecutionError) as caught:
        await engine.create(
            [
                "--mount",
                (
                    "type=bind,source=/private/credential,"
                    "target=/run/credentials/runtime,readonly"
                ),
                "image",
            ]
        )

    assert "/private/credential" not in str(caught.value)
    assert "<private-host-path>" in str(caught.value)
