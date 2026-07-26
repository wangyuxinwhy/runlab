from runlab.stats import MeasurementAccumulator, parse_pair, parse_size


def test_docker_size_units() -> None:
    assert parse_size("1.5KiB") == 1536
    assert parse_size("2 MB") == 2_000_000
    assert parse_pair("10kB / 2MB") == (10_000, 2_000_000)


def test_accumulator_keeps_peaks_and_maximum_cumulative_io() -> None:
    accumulator = MeasurementAccumulator()
    accumulator.add(
        {
            "CPUPerc": "1.5%",
            "MemUsage": "10MiB / 1GiB",
            "PIDs": "3",
            "NetIO": "1kB / 2kB",
            "BlockIO": "5kB / 6kB",
        }
    )
    accumulator.add(
        {
            "CPUPerc": "0.5%",
            "MemUsage": "12MiB / 1GiB",
            "PIDs": "2",
            "NetIO": "3kB / 4kB",
            "BlockIO": "7kB / 8kB",
        }
    )
    accumulator.add(
        {
            "CPUPerc": "0%",
            "MemUsage": "0B / 0B",
            "PIDs": "0",
            "NetIO": "0B / 0B",
            "BlockIO": "0B / 0B",
        }
    )

    result = accumulator.finish(2.0)
    assert result.peak_cpu_percent == 1.5
    assert result.peak_memory_bytes == 12 * 1024 * 1024
    assert result.peak_pids == 3
    assert result.network_rx_bytes == 3000
    assert result.block_read_bytes == 7000
    assert result.block_write_bytes == 8000
    assert result.samples == 3
