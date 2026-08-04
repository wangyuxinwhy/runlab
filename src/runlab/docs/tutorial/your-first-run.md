# Your First Run

By the end of this page you will have executed a real Agent inside a container and inspected the record it left behind. It takes about ten minutes, most of which is one image build.

## What you need

Docker running locally, [uv](https://docs.astral.sh/uv/), and an API key for a model provider. The example Base runs [pi](https://www.npmjs.com/package/@earendil-works/pi-coding-agent) against DeepSeek, so this walkthrough uses a DeepSeek key.

```bash
git clone <your-clone-url> runlab
cd runlab
uv sync
```

## Give RunLab the credential

RunLab never creates or reads a credential store on its own. You materialize a private directory whose entries match the names a Base declares, and RunLab mounts them read-only.

The example Base declares one slot named `deepseek`, so create a file with exactly that name:

```bash
mkdir -p ~/.config/runlab/credentials
chmod 700 ~/.config/runlab/credentials
printf '%s' "$DEEPSEEK_API_KEY" > ~/.config/runlab/credentials/deepseek
chmod 600 ~/.config/runlab/credentials/deepseek
```

The permissions matter. RunLab refuses to accept a Run when the store or any entry is readable by group or others.

## Look at the Base before building it

A Base owns one Agent runtime and the contract its entrypoint honors: read the instruction from stdin, work in `/workspace`, write deliverables to `/artifacts`, exit nonzero on failure.

```bash
uv run runlab base check examples/bases/pi
```

```json
{"name":"pi","declaration":"sha256:1232d467…","output_protocol":"pi-session-jsonl","logs":"/root/.pi/agent/sessions","credentials":["deepseek"],"inputs":[],"locked":false}
```

Two fields are worth pausing on. `declaration` is the content digest of the directory — it identifies the *source*. And `locked` is `false`, meaning this declaration has not yet been fixed into anything you could run twice.

## Fix it into a realization

```bash
uv run runlab base build examples/bases/pi
```

The first build takes around a minute. When it finishes:

```json
{"name":"pi","declaration":"sha256:1232d467…","platform":"linux/arm64","realization":"sha256:6aca1665…","lock":"…/examples/bases/pi/base.lock"}
```

`realization` is the image that actually exists, and it is now recorded in `base.lock`:

```json
{
  "schema_version": 1,
  "declaration": "sha256:1232d467…",
  "realizations": { "linux/arm64": "sha256:6aca1665…" }
}
```

Commit that lock file. It is what makes this environment a constant rather than something rebuilt slightly differently every time. Run the same command again and it returns in well under a second, reporting the same realization instead of building a new one.

## Run a Task

The Task is one instruction plus optional material. This one asks for a single file:

```bash
cat examples/tasks/hello/task.md
```

```text
Write the file `/artifacts/answer.md`.

Its entire content must be the single word `PONG` followed by a newline. …
```

Execute it:

```bash
uv run runlab run start \
  --base examples/bases/pi \
  --task examples/tasks/hello \
  --output runs \
  --timeout-seconds 180
```

Progress goes to stderr; stdout is one JSON object:

```json
{"run_id":"run:8f3f828efee0","outcome":"succeeded","environment":"sha256:17f50925…","record":"…/runs/run-8f3f828efee0/run.json"}
```

## Read what it kept

```bash
cd runs/run-8f3f828efee0
cat artifacts/answer.md      # PONG
find . -type f | sort
```

```text
./artifacts/answer.md
./inputs/base/Dockerfile
./inputs/base/base.json
./inputs/base/base.lock
./inputs/base/entrypoint.sh
./inputs/task/task.md
./logs/runtime/2026-…-019fcbf8….jsonl
./logs/stderr.log
./logs/stdout.log
./logs/task.md
./run.json
```

`inputs/` holds byte copies of the declarations, not just their digests — a digest can prove two things differ but cannot reconstruct either, so a Run that only recorded digests would stop being usable the moment you edited the source directory.

`logs/runtime/` holds pi's own session file, untouched. RunLab derives normalized token counts from it but keeps the original, so any derived number can be recomputed:

```bash
python3 -c "
import json; r = json.load(open('run.json'))
print(r['measurements']['model_usage'])
print(r['spec']['environment']['digest'])
"
```

```text
{'input_tokens': 1684, 'cached_input_tokens': 1664, 'cache_write_input_tokens': 0, 'output_tokens': 127, 'reasoning_output_tokens': 33}
sha256:17f50925…
```

That last value is the `environment` digest — the whole realization chain folded into one key. Two Runs are comparable exactly when it matches.

## What you have

One immutable Run directory that can be read, moved, and compared later, and a lock file that will hand you the same environment next week.

Next, [your first ablation](your-first-ablation.md) changes exactly one thing across three Runs and shows why that is harder than it sounds.
