# Query Runs

Use `run list` for a short glance at recent Runs, `query run` to select or aggregate Runs, and `run get` for one complete persisted Run record. Querying never starts a Run, refreshes State, repairs a record, or changes the database.

## Discover the contract first

Do not guess relation or column names. The schema is bundled with the same binary that executes the query:

```bash
runlab schema list
runlab schema get runs
```

The current public SQL surface contains one Relation, `runs`. It exposes accepted caller metadata, Initial Image identity, lifecycle, and a small set of terminal outcome facts. It deliberately omits complete Run input, captured streams, errors, and Final Environments; read those with `runlab run get RUN_ID`.

`initial_image_name` is the Catalog name used when RunLab accepted the Run. It is `NULL` when the caller selected the Image by digest or when an older record predates this accepted fact. `initial_image_digest` is the immutable content identity. `labels` is a JSON object whose keys and values are caller-defined strings.

## Select Runs with ordinary SQL

Find recent SWE-bench Runs made with the Pi Image:

```bash
runlab query run --stdin <<'SQL'
SELECT run_id, accepted_at, description, primary_exit_code
FROM runs
WHERE initial_image_name = 'pi'
  AND json_extract(labels, '$.suite') = 'swe-bench'
ORDER BY accepted_at DESC
LIMIT 10;
SQL
```

Count outcomes without returning every Run:

```bash
runlab query run \
  "SELECT primary_process_kind, primary_exit_code, COUNT(*) AS runs FROM runs GROUP BY primary_process_kind, primary_exit_code"
```

Use `--file QUERY.sql` for a checked-in or generated query. Inline SQL, `--file`, and `--stdin` are mutually exclusive.

## Treat bounds as part of the result

Every query is bounded by row count, cell bytes, total serialized row bytes, and time. The JSON result states `complete: false` with `incomplete_reason: "row_limit"` or `"output_budget"` when output was cut off, and reports `cells_truncated` when individual values were shortened. Increase the corresponding bound only when the workflow needs more data; aggregate or narrow before returning large row sets.

Only one read-only statement against public Relations is allowed. Private storage tables, schema tables, attached databases, writes, and extension loading are not part of the interface.
