from __future__ import annotations

import asyncio
import hashlib
import json
import os
import shutil
import subprocess
from collections import defaultdict
from pathlib import Path
from typing import Any

from filelock import AsyncFileLock

from runlab.errors import ExecutionError


class DockerEngine:
    def __init__(self) -> None:
        self._build_locks: defaultdict[str, asyncio.Lock] = defaultdict(asyncio.Lock)
        executable = shutil.which("docker")
        if executable is None:
            msg = "Docker executable is not available"
            raise ExecutionError(msg)
        self._executable = executable

    @property
    def executable(self) -> str:
        return self._executable

    async def check(self) -> str:
        return (await self.run("version", "--format", "{{.Server.Version}}")).strip()

    async def ensure_image(
        self,
        context: Path,
        tag: str,
        *,
        build_contexts: dict[str, Path] | None = None,
    ) -> str:
        """Treat concurrent creation of the same content tag as successful reuse."""

        async with self._build_locks[tag], _build_lock(tag):
            return await self._ensure_image_locked(context, tag, build_contexts)

    async def _ensure_image_locked(
        self,
        context: Path,
        tag: str,
        build_contexts: dict[str, Path] | None,
    ) -> str:
        image_id = await self.image_id(tag, required=False)
        if image_id is None:
            arguments = ["build", "--quiet", "--tag", tag]
            for name, path in sorted((build_contexts or {}).items()):
                arguments.extend(["--build-context", f"{name}={path}"])
            arguments.append(".")
            try:
                await self.run(*arguments, cwd=context)
            except ExecutionError:
                image_id = await self.image_id(tag, required=False)
                if image_id is None:
                    raise
        resolved = await self.image_id(tag, required=True)
        if resolved is None:
            msg = "Docker image disappeared after a successful build"
            raise ExecutionError(msg)
        return resolved

    async def image_id(self, tag: str, *, required: bool) -> str | None:
        try:
            return (
                await self.run("image", "inspect", "--format", "{{.Id}}", tag)
            ).strip()
        except ExecutionError:
            if not required:
                return None
            raise

    async def create(self, arguments: list[str]) -> str:
        private_values = _mount_sources(arguments)
        try:
            return (await self.run("create", *arguments)).strip()
        except ExecutionError as error:
            detail = _redact(str(error), private_values)
            raise ExecutionError(detail) from error

    async def stop(self, container: str) -> None:
        await self.run("stop", "--time", "2", container)

    async def remove(self, container: str) -> None:
        await self.run("rm", "--force", container)

    async def inspect_state(self, container: str) -> dict[str, Any]:
        raw = await self.run("inspect", "--format", "{{json .State}}", container)
        return json.loads(raw)

    async def stats(self, container: str) -> dict[str, str]:
        raw = await self.run(
            "stats", "--no-stream", "--format", "{{json .}}", container
        )
        return json.loads(raw)

    async def run(self, *arguments: str, cwd: Path | None = None) -> str:
        """Execute Docker without leaking arguments into public errors."""

        def invoke() -> subprocess.CompletedProcess[str]:
            return subprocess.run(  # noqa: S603
                [self._executable, *arguments],
                cwd=cwd,
                text=True,
                capture_output=True,
                check=False,
            )

        try:
            completed = await asyncio.to_thread(invoke)
        except OSError as error:
            msg = "could not execute Docker"
            raise ExecutionError(msg) from error
        if completed.returncode != 0:
            detail = (
                completed.stderr.strip()
                or completed.stdout.strip()
                or "unknown Docker error"
            )
            msg = f"Docker operation failed: {detail}"
            raise ExecutionError(msg)
        return completed.stdout


def _build_lock(tag: str) -> AsyncFileLock:
    cache = Path(os.environ.get("XDG_CACHE_HOME", "~/.cache")).expanduser()
    directory = cache / "runlab" / "locks"
    directory.mkdir(parents=True, exist_ok=True)
    name = hashlib.sha256(tag.encode()).hexdigest()
    return AsyncFileLock(directory / f"{name}.lock")


def _mount_sources(arguments: list[str]) -> tuple[str, ...]:
    sources: list[str] = []
    for argument in arguments:
        if not argument.startswith("type=bind,source="):
            continue
        source, separator, _remainder = argument.removeprefix(
            "type=bind,source="
        ).partition(",target=")
        if separator:
            sources.append(source)
    return tuple(sources)


def _redact(value: str, private_values: tuple[str, ...]) -> str:
    for private in private_values:
        value = value.replace(private, "<private-host-path>")
    return value
