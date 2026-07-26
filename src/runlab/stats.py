from __future__ import annotations

import re
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation

from runlab.models import Measurements

_SIZE_PATTERN = re.compile(r"^\s*([0-9.]+)\s*([A-Za-z]+)\s*$")
_UNIT_MULTIPLIERS = {
    "B": 1,
    "KB": 1_000,
    "MB": 1_000_000,
    "GB": 1_000_000_000,
    "TB": 1_000_000_000_000,
    "KIB": 1_024,
    "MIB": 1_048_576,
    "GIB": 1_073_741_824,
    "TIB": 1_099_511_627_776,
}


def parse_size(value: str) -> int:
    match = _SIZE_PATTERN.fullmatch(value)
    if match is None:
        msg = f"unsupported Docker size: {value!r}"
        raise ValueError(msg)
    number, unit = match.groups()
    try:
        return int(Decimal(number) * _UNIT_MULTIPLIERS[unit.upper()])
    except (InvalidOperation, KeyError) as error:
        msg = f"unsupported Docker size: {value!r}"
        raise ValueError(msg) from error


def parse_percent(value: str) -> float:
    return float(value.strip().removesuffix("%"))


def parse_pair(value: str) -> tuple[int, int]:
    left, separator, right = value.partition("/")
    if not separator:
        msg = f"unsupported Docker size pair: {value!r}"
        raise ValueError(msg)
    return parse_size(left), parse_size(right)


@dataclass(slots=True)
class MeasurementAccumulator:
    peak_cpu_percent: float | None = None
    peak_memory_bytes: int | None = None
    peak_pids: int | None = None
    network_rx_bytes: int | None = None
    network_tx_bytes: int | None = None
    block_read_bytes: int | None = None
    block_write_bytes: int | None = None
    samples: int = 0

    def add(self, sample: dict[str, str]) -> None:
        cpu = parse_percent(sample["CPUPerc"])
        memory, _limit = parse_pair(sample["MemUsage"])
        pids = int(sample["PIDs"])
        network_rx, network_tx = parse_pair(sample["NetIO"])
        block_read, block_write = parse_pair(sample["BlockIO"])
        self.peak_cpu_percent = _maximum(self.peak_cpu_percent, cpu)
        self.peak_memory_bytes = _maximum(self.peak_memory_bytes, memory)
        self.peak_pids = _maximum(self.peak_pids, pids)
        self.network_rx_bytes = _maximum(self.network_rx_bytes, network_rx)
        self.network_tx_bytes = _maximum(self.network_tx_bytes, network_tx)
        self.block_read_bytes = _maximum(self.block_read_bytes, block_read)
        self.block_write_bytes = _maximum(self.block_write_bytes, block_write)
        self.samples += 1

    def finish(self, wall_seconds: float) -> Measurements:
        return Measurements(
            wall_seconds=wall_seconds,
            peak_cpu_percent=self.peak_cpu_percent,
            peak_memory_bytes=self.peak_memory_bytes,
            peak_pids=self.peak_pids,
            network_rx_bytes=self.network_rx_bytes,
            network_tx_bytes=self.network_tx_bytes,
            block_read_bytes=self.block_read_bytes,
            block_write_bytes=self.block_write_bytes,
            samples=self.samples,
        )


def _maximum[NumberT: (int, float)](
    current: NumberT | None, candidate: NumberT
) -> NumberT:
    return candidate if current is None else max(current, candidate)
