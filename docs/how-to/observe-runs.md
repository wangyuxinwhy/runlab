# Observe Terminal Runs

RunLab has two separate observation surfaces:

- A **Live Event** is an ephemeral NDJSON event emitted on stderr while `exec` or foreground `run start` is active. It carries execution progress, is not a persistent Run asset, and is not replayable.
- An **Observation** is an immutable typed record appended to a terminal Run. It is queryable, participates in checked Run deletion, and can be corrected or retracted without erasing history.

Observation belongs to the RunLab product, not the Run Protocol or `RunEngine`. RunLab validates the document against a registered Observation Type and stores it. The external Method owns source discovery, source parsing, semantic derivation, and whether available evidence is sufficient to produce a payload. RunLab records the Method's declared name and version but does not know or record all of its inputs.

## Discover or register a Type

Every Observation Type is an immutable definition with exactly five fields:

```json
{
  "schema_version": 1,
  "type": "example/rubric_score@v1",
  "title": "Rubric score",
  "description": "A score from zero through one produced by the declared rubric Method.",
  "payload_schema": {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "additionalProperties": false,
    "required": ["score"],
    "properties": {
      "score": {"type": "number", "minimum": 0, "maximum": 1}
    }
  }
}
```

`description` is one string containing the Type's complete semantic contract. `payload_schema` is a self-contained JSON Schema Draft 2020-12 object. Schema compilation is offline, so definitions cannot depend on network-fetched references.

The Type identity uses `namespace/name@vN`. Registration is create-only: retrying the same definition is idempotent, while reusing an identity for a different definition is a conflict. The `runlab/` namespace is reserved for built-in Types.

```bash
runlab observation type list
runlab observation type get runlab/token_usage@v1
runlab observation type register --document rubric-score-type.json
```

Built-in and externally registered Types are rows in the same registry. They use the same schema validator, Observation table, correction rules, and query path. RunLab does not add Type-specific columns or validators.

## Build and submit an Observation

After an external Method has produced a payload, assign a canonical lowercase UUID v4 and build a document with no additional fields:

```json
{
  "schema_version": 1,
  "observation_id": "550e8400-e29b-41d4-a716-446655440010",
  "run_id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "example/rubric_score@v1",
  "method": {
    "name": "example/rubric-grader",
    "version": "1.0.0"
  },
  "payload": {
    "score": 0.8
  }
}
```

The input has no `kind`: the CLI command already determines that this is an Observation document, while `schema_version` versions its structure. The document is limited to 1 MiB.

```bash
runlab observation submit --document observation.json
runlab observation submit --document - <observation.json
```

The target Run must be terminal and the Type must already be registered. The payload must satisfy that Type's `payload_schema`. The caller owns `observation_id`; retrying the same semantic document returns `created: false`, while reusing the UUID for different content is a conflict. RunLab assigns `submitted_at`.

## Query generic payloads

Discover the registry and Observation Relations:

```bash
runlab schema get observation_types
runlab schema get observations
```

The `observations` Relation exposes only common fields. Select Type-specific values with SQLite JSON functions:

```bash
runlab query run \
  "SELECT observation_id, run_id, json_extract(payload, '$.score') AS score FROM observations WHERE state = 'active' AND type = 'example/rubric_score@v1' ORDER BY submitted_at DESC LIMIT 100"
```

The Relation retains all states. `active` is current, `superseded` has an accepted replacement, and `retracted` has an accepted retraction. Query `observation_retractions` for the append-only retraction records themselves.

## Built-in token usage Type

`runlab/token_usage@v1` is pre-registered as an ordinary Type. Inspect its current definition with `runlab observation type get runlab/token_usage@v1`. Its payload is:

```json
{
  "coverage": "complete",
  "input_tokens": 12000,
  "cached_input_tokens": 8000,
  "cache_write_input_tokens": null,
  "output_tokens": 3400,
  "reasoning_output_tokens": 900
}
```

`input_tokens` includes ordinary input, cache reads, and cache writes. `cached_input_tokens` and `cache_write_input_tokens` are optional reported subsets. `output_tokens` includes reasoning output, and `reasoning_output_tokens` is an optional reported subset. A `null` subset means unknown, not zero.

`coverage: "complete"` means the Method established complete cumulative input and output coverage for the Run. `coverage: "partial"` means the reported counts are a reliable known lower bound. If cumulative input or output usage is unavailable, the Method must not submit this Type. Total tokens are derived in a query as input plus output and are not duplicated in the payload:

```bash
runlab query run \
  "SELECT observation_id, json_extract(payload, '$.input_tokens') + json_extract(payload, '$.output_tokens') AS total_tokens FROM observations WHERE state = 'active' AND type = 'runlab/token_usage@v1'"
```

## Correct or retract without rewriting history

To correct an active Observation, submit a new Observation with a new UUID and add:

```json
"supersedes_observation_id": "550e8400-e29b-41d4-a716-446655440010"
```

The predecessor must exist, be active, and have the same Run and Type. Only one replacement is accepted for a predecessor.

To withdraw an active Observation without a replacement, submit:

```json
{
  "schema_version": 1,
  "retraction_id": "550e8400-e29b-41d4-a716-446655440011",
  "observation_id": "550e8400-e29b-41d4-a716-446655440010",
  "reason": "the Method configuration was later shown to be invalid"
}
```

```bash
runlab observation retract --document retraction.json
```

A retraction is idempotent by `retraction_id`. A superseded or already retracted Observation is inactive and cannot receive another retraction.

## Delete a Run through a fresh checked plan

Observations and retractions are assets owned by their Run. `run delete check` includes their count, encoded byte estimate, and content fingerprint in deletion-plan schema version 2. Adding, correcting, or retracting an Observation after check makes that plan stale. A successful apply atomically deletes the Run Record and its Observation history, then retains the Run tombstone. Registered Type definitions are State-level contracts and are not deleted with a Run.
