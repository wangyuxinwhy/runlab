# Drive a Matrix from a Script

RunLab executes one Run per invocation and has no matrix runner. Selection, repeats, ordering, and scheduling live in your script, where they can be as adaptive as the experiment needs.

## Fix every realization first

Build before you loop. A build inside the loop mixes image construction failures into Agent results and pays for a cold cache in the first iteration's wall time.

```bash
uv run runlab base build bases/pi
for o in overlays/*/; do
  [ -f "$o/Dockerfile" ] && uv run runlab overlay build "$o" --base bases/pi
done
```

After this, every `run start` either resolves to a fixed realization or fails loudly.

## Loop

```bash
#!/usr/bin/env bash
set -euo pipefail

OUT=runs/skill-ablation
mkdir -p "$OUT"

for overlay in baseline skills-v1 skills-v2; do
  for task in tasks/*/; do
    for repeat in 1 2 3; do
      uv run runlab run start \
        --base bases/pi \
        --overlay "overlays/$overlay" \
        --task "$task" \
        --output "$OUT" \
        --timeout-seconds 300 \
      || echo "infrastructure failure: $overlay $task #$repeat" >&2
    done
  done
done
```

Repeats belong here rather than in RunLab. They are how you separate model non-determinism from the variable you are studying, and how many you need is a property of the experiment.

The directory layout is your grouping mechanism. RunLab has no experiment entity, so `runs/skill-ablation/` *is* the experiment.

## Distinguish the two failure kinds

`run start` exits `1` when the outcome is `setup_failed` or `collection_failed`, and `0` for `agent_failed` and `timed_out`.

That split is deliberate. An Agent failing a task is a result you want to keep and count. Infrastructure failing is a problem with your setup, and treating the two the same is how a broken credential quietly becomes "the Agent performs poorly".

## Read the results

Every Run is self-describing, so analysis is a directory walk with no index to maintain:

```bash
python3 - <<'PY'
import json, pathlib, collections

rows = []
for run in sorted(pathlib.Path("runs/skill-ablation").glob("run-*")):
    r = json.load(open(run / "run.json"))
    env = r["spec"]["environment"]
    rows.append({
        "env": env["digest"],
        "overlays": tuple(o["name"] for o in env["overlays"]),
        "task": r["spec"]["task"]["name"],
        "outcome": r["outcome"],
        "output_tokens": (r["measurements"]["model_usage"] or {}).get("output_tokens"),
        "path": str(run),
    })

by_cell = collections.defaultdict(list)
for row in rows:
    by_cell[(row["overlays"], row["task"])].append(row)

for (overlays, task), cell in sorted(by_cell.items()):
    outcomes = collections.Counter(r["outcome"] for r in cell)
    print(f"{overlays or '(none)'} × {task}: n={len(cell)} {dict(outcomes)}")
PY
```

## Verify the comparison before believing it

Group by the `environment` digest, not by the Overlay name you passed. If two cells you meant to compare share a Base realization and a Task digest, the difference is attributable; if they do not, something drifted and the comparison is void.

```python
assert len({r["spec"]["environment"]["base"]["realization"] for r in records}) == 1
assert len({r["spec"]["task"]["digest"] for r in records}) == 1
```

Making that check explicit costs two lines and is the only thing standing between an ablation and a plausible-looking artifact of an uncontrolled change.

## Keep the failures

Do not filter failed Runs out of the directory before analysis. Every accepted Run keeps a terminal record precisely so the distribution you compute reflects everything that ran, not everything that happened to succeed.
