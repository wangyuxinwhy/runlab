# Your First Ablation

An ablation runs the same Task in environments that differ in exactly one way. This page runs three, then shows how the records prove that only one thing moved — which is the part that usually goes wrong.

It assumes you finished [your first Run](your-first-run.md), so the Base is already locked.

## The question

Does a skill change what the Agent produces? To answer it you need a Task whose output depends on the skill rather than on the instruction.

```bash
cat examples/tasks/greet/task.md
```

```text
Write the file `/artifacts/answer.md`.

Its entire content must be the project greeting word defined by your available skills, followed by a newline. If no skill defines a greeting word, write `UNKNOWN` instead. …
```

The instruction never names a greeting. Two Overlays supply competing definitions:

```bash
cat examples/overlays/skills-v1/files/skills/greeting/SKILL.md
```

```text
The project greeting word is `HELLO`. …
```

`skills-v2` is identical except that it says `GREETINGS`. Both declare the same mount:

```json
{
  "name": "skills-v1",
  "mounts": [{ "source": "files/skills", "target": "/opt/runlab/skills" }]
}
```

Neither mentions a pi command-line flag. The Base entrypoint decides how to consume anything mounted at that path, which is what keeps the execution contract owned by the Base.

## Run the three arms

```bash
for arm in baseline skills-v1 skills-v2; do
  uv run runlab run start \
    --base examples/bases/pi \
    --overlay "examples/overlays/$arm" \
    --task examples/tasks/greet \
    --output runs --timeout-seconds 180
done
```

`baseline` is an empty Overlay — it declares nothing at all. It exists so that the control arm has the same shape as the others in your script.

## Read the results

```bash
python3 - <<'PY'
import json, pathlib
for d in sorted(pathlib.Path("runs").glob("run-*")):
    r = json.load(open(d / "run.json"))
    if r["spec"]["task"]["name"] != "greet":
        continue
    env = r["spec"]["environment"]
    answer = (d / "artifacts" / "answer.md")
    print(f"{[o['name'] for o in env['overlays']] or '(none)':<14}"
          f" answer={answer.read_text().strip():<10}"
          f" base={env['base']['realization'][:16]}"
          f" task={r['spec']['task']['digest'][:16]}"
          f" env={env['digest'][:16]}")
PY
```

```text
(none)         answer=UNKNOWN    base=sha256:6aca166 task=sha256:bd23f77 env=sha256:441287b
['skills-v1']  answer=HELLO      base=sha256:6aca166 task=sha256:bd23f77 env=sha256:26420cf
['skills-v2']  answer=GREETINGS  base=sha256:6aca166 task=sha256:bd23f77 env=sha256:486b651
```

Three things to notice.

The **answers differ**, so the skill did change the output.

The **Base realization and Task digest are identical** across all three. Not "the same declaration" — the same built image and the same task bytes. Nothing drifted underneath the comparison.

The **environment digest differs** in all three, and the Overlay chain is the only field that produced that difference. The record leaves exactly one candidate explanation, which is what makes the result attributable.

## Where the empty Overlay went

The `baseline` arm shows `(none)`, not `['baseline']`. An Overlay that changes nothing describes the same environment as no Overlay at all, so it normalizes away.

You can check that this is real rather than cosmetic:

```bash
uv run runlab run start --base examples/bases/pi --overlay examples/overlays/baseline \
  --task examples/tasks/greet --output runs --timeout-seconds 180
uv run runlab run start --base examples/bases/pi \
  --task examples/tasks/greet --output runs --timeout-seconds 180
```

Both report the same `environment` digest. Had they differed, you would have two declarations behind one realization, and comparability judgments would need a special case for it forever after.

## Try a different form

Skills arrive as mounted files, but capability can also arrive as installed software. The `with-python` Overlay adds an interpreter the Base image does not ship:

```bash
uv run runlab overlay build examples/overlays/with-python --base examples/bases/pi

uv run runlab run start --base examples/bases/pi \
  --task examples/tasks/probe-python --output runs --timeout-seconds 240
uv run runlab run start --base examples/bases/pi --overlay examples/overlays/with-python \
  --task examples/tasks/probe-python --output runs --timeout-seconds 240
```

The answers are `NO-PYTHON` and `PYTHON`. This Overlay produced a new image rather than a mount, so it has an `overlay.lock` keyed by the Base realization it was built on — but from the experiment's point of view it is the same kind of variable as the skill, recorded in the same field.

That equivalence is the whole reason Overlay exists as its own layer. See [Why the layers exist](../explanation/why-layers.md) for what breaks without it.

## Next

[Drive a matrix from a script](../how-to/drive-a-matrix.md) turns this into something you can run over many Tasks, and [Author an Overlay](../how-to/author-an-overlay.md) covers the remaining delivery forms.
