---
title: CLI Output Contract
description: Final JSON, Live Event, error, and bounded-query output channels.
---

# CLI Output Contract

RunLab keeps machine-readable outputs separated by responsibility:

- Successful non-streaming commands write one JSON value to stdout.
- Command failures write one `runlab.error` JSON object to stderr and return nonzero.
- Foreground `run start` and `exec` write ephemeral NDJSON Live Events to stderr while reserving stdout for the final JSON result.
- Program stdout and stderr are execution facts carried by `program.stdout` and `program.stderr` Live Events; they are not RunLab command errors.
- Bounded query responses explicitly report row, cell, byte, or time truncation.

Persistent Run facts are read with `run get` or the public query plane. Live Events are not replayable and do not replace terminal Run publication.
