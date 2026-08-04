"""Docker particulars: building images and running one attached container.

This package owns every assumption about how Docker is invoked and how it
reports state. Callers receive plain values and never see a Docker flag.
"""

import asyncio
import hashlib
import json
import os
import shutil
import subprocess
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

from filelock import AsyncFileLock

from runlab.core.errors import ExecutionError


@dataclass(frozen=True, slots=True)
class BuildRequest:
    context: Path
    tag: str
    dockerfile: Path | None = None
    build_args: dict[str, str] | None = None
    build_contexts: dict[str, Path] | None = None


@dataclass(frozen=True, slots=True)
class ContainerState:
    exit_code: int
    oom_killed: bool
    error: str | None


class DockerEngine:
    def __init__(self) -> None:
        executable = shutil.which("docker")
        if executable is None:
            message = "the Docker executable is not available"
            raise ExecutionError(message)
        self._executable = executable
        self._build_locks: defaultdict[str, asyncio.Lock] = defaultdict(asyncio.Lock)

    @property
    def executable(self) -> str:
        return self._executable

    async def version(self) -> str:
        return (await self.run("version", "--format", "{{.Server.Version}}")).strip()

    async def platform(self) -> str:
        """Report the daemon's platform, which selects the image a tag resolves to."""
        raw = await self.run("version", "--format", "{{.Server.Os}}/{{.Server.Arch}}")
        return raw.strip()

    async def image_id(self, tag: str, /) -> str | None:
        """Return the platform-specific image identity, or None when absent."""
        try:
            raw = await self.run("image", "inspect", "--format", "{{.Id}}", tag)
        except ExecutionError:
            return None
        return raw.strip() or None

    async def ensure_reference(self, image: str, /) -> str:
        """Give an image identity a locally resolvable name.

        A builder treats a bare digest in a `FROM` instruction as a registry
        reference and tries to pull it, so an image that exists only locally
        needs a name before another image can be built on top of it.
        """
        reference = f"runlab-image:{image.removeprefix('sha256:')[:16]}"
        await self.run("tag", image, reference)
        return reference

    async def build(self, request: BuildRequest, /) -> str:
        """Build one image and return its identity.

        Concurrent callers of the same tag are serialized in-process and across
        processes, because a matrix driven by an Agent script runs many Runs
        that legitimately share one image.
        """
        async with self._build_locks[request.tag], _build_lock(request.tag):
            arguments = ["build", "--quiet", "--tag", request.tag]
            if request.dockerfile is not None:
                arguments.extend(["--file", str(request.dockerfile)])
            for name, value in sorted((request.build_args or {}).items()):
                arguments.extend(["--build-arg", f"{name}={value}"])
            for name, path in sorted((request.build_contexts or {}).items()):
                arguments.extend(["--build-context", f"{name}={path}"])
            arguments.append(".")
            await self.run(*arguments, cwd=request.context)
            identity = await self.image_id(request.tag)
            if identity is None:
                message = "the Docker image disappeared after a successful build"
                raise ExecutionError(message)
            return identity

    async def create(self, arguments: list[str], /) -> str:
        private_values = _mount_sources(arguments)
        try:
            return (await self.run("create", *arguments)).strip()
        except ExecutionError as error:
            raise ExecutionError(_redact(str(error), private_values)) from error

    async def stop(self, container: str, /) -> None:
        await self.run("stop", "--time", "2", container)

    async def remove(self, container: str, /) -> None:
        await self.run("rm", "--force", container)

    async def inspect_state(self, container: str, /) -> ContainerState:
        raw = await self.run("inspect", "--format", "{{json .State}}", container)
        state: dict[str, object] = json.loads(raw)
        return ContainerState(
            exit_code=int(str(state.get("ExitCode", 0))),
            oom_killed=bool(state.get("OOMKilled", False)),
            error=str(state.get("Error") or "") or None,
        )

    async def start_attached(
        self,
        container: str,
        /,
        *,
        stdin_bytes: bytes,
        stdout_path: Path,
        stderr_path: Path,
        timeout_seconds: int,
    ) -> tuple[int, bool]:
        """Run the container to completion, returning the client exit and timeout flag.

        The Task instruction arrives on stdin because that is the fixed Base
        contract; nothing about the instruction reaches the command line.
        """
        timed_out = False
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            child = await asyncio.create_subprocess_exec(
                self._executable,
                "start",
                "--attach",
                "--interactive",
                container,
                stdin=asyncio.subprocess.PIPE,
                stdout=stdout,
                stderr=stderr,
            )
            if child.stdin is None:
                message = "the attached Docker client did not expose stdin"
                raise ExecutionError(message)
            child.stdin.write(stdin_bytes)
            await child.stdin.drain()
            child.stdin.close()
            try:
                async with asyncio.timeout(timeout_seconds):
                    client_exit = await child.wait()
            except TimeoutError:
                timed_out = True
                await self.stop(container)
                client_exit = await child.wait()
        return client_exit, timed_out

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
            message = "could not execute Docker"
            raise ExecutionError(message) from error
        if completed.returncode != 0:
            detail = (
                completed.stderr.strip()
                or completed.stdout.strip()
                or "unknown Docker error"
            )
            message = f"Docker operation failed: {detail}"
            raise ExecutionError(message)
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
        source, separator, _rest = argument.removeprefix("type=bind,source=").partition(
            ",target="
        )
        if separator:
            sources.append(source)
    return tuple(sources)


def _redact(value: str, private_values: tuple[str, ...]) -> str:
    for private in private_values:
        value = value.replace(private, "<private-host-path>")
    return value
