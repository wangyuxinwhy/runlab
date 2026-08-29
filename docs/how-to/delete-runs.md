# Delete Terminal Runs

Run deletion permanently removes complete terminal Run Records. It does not decide which Runs are expired, implement a TTL, delete only part of a record, erase OCI content, VACUUM SQLite, or claim secure erasure. The caller owns the retention judgment.

Accepted Runs cannot be deleted. Try `runlab run reconcile RUN_ID` first. A dead coordinator can sometimes be reconciled to a terminal interruption when durable evidence proves the Engine never started. A Run whose reconciliation outcome is `evidence_incomplete` remains accepted and has no deletion override.

## Select exact identities

Discover the public columns, then select a bounded set. RFC 3339 timestamps are text facts and must not be compared lexically. Use `terminal_unix_seconds` for the range and retain exact `terminal_at` values for review:

```bash
runlab query run --stdin >selection.json <<'SQL'
SELECT run_id, terminal_at
FROM runs
WHERE lifecycle = 'terminal'
  AND terminal_unix_seconds < unixepoch('now', '-1 day', 'subsec')
ORDER BY terminal_unix_seconds ASC, run_id ASC;
SQL

jq -r '.rows[].run_id' selection.json >run-ids.txt
```

Confirm that the query result says `complete: true`. Deletion accepts at most 1000 newline-delimited canonical UUIDs so a caller must explicitly page a larger selection.

## Preview the content consequence

If content recovery is the reason for deletion, ask storage management what would become unreachable before making the irreversible change:

```bash
runlab storage prune check --without-runs run-ids.txt
```

This is a read-only hypothetical calculation. It excludes only existing terminal Runs from the retention roots and leaves every Run, Catalog entry, OCI blob, and snapshot untouched. Catalog and remaining Run roots can make the hypothetical reclaimable amount zero.

## Freeze and review the database plan

Create one canonical UUID v4 for the deletion intent. The caller owns this `operation_id` and must reuse it when retrying the same intent:

```bash
operation_id=$(uuidgen | tr '[:upper:]' '[:lower:]')

runlab run delete check \
  --operation-id "$operation_id" \
  --ids run-ids.txt >delete-plan.json
```

`check` writes every candidate's exact `terminal_at`, logical record bytes, and database record fingerprint. It also reports every Program Final Image that is still named by the Catalog; deletion is allowed, but the Catalog Image will no longer have this Run provenance link.

`not_terminal` and `not_found` make the plan ineligible. A `not_terminal` entry includes the exact `runlab run reconcile RUN_ID` recovery command. `already_deleted` is non-blocking and is excluded from the candidate set so rerunning the complete ID workflow converges.

The plan has no aggregate digest. JSON parsing rejects truncation, and apply verifies each candidate fingerprint against the database. Filesystem predictions are deliberately absent because concurrent OCI changes do not invalidate a database deletion plan.

## Apply atomically

```bash
runlab run delete apply --plan delete-plan.json
```

Apply takes the normal shared State lease and a short `BEGIN IMMEDIATE` SQLite transaction. It recomputes every candidate fingerprint, inserts tombstones, and deletes the complete batch or none of it. A concurrent database writer can return a retryable `conflict`; retry the same plan and operation ID. If commit succeeded but stdout was lost, the retry returns `already_applied`.

For a direct pipe when a separate plan artifact is unnecessary:

```bash
runlab run delete check --operation-id "$operation_id" --ids run-ids.txt \
  | runlab run delete apply --plan -
```

Deleted identities remain visible through `run_deletions`. `run get` reports that the identity was deleted, and `run start` permanently rejects its reuse. Database pages may remain allocated because deletion does not run VACUUM.

## Reclaim unreferenced content separately

```bash
runlab storage prune check
runlab storage prune apply
```

Run deletion and filesystem pruning are intentionally separate transactions. Prune reports the authoritative post-deletion OCI and snapshot effect. It refuses to remove any file if the retained reference graph is incomplete. Removing snapshot chains can recover substantial space, but makes later warm-start timing evidence cold-cache evidence and must be reported as such.

`reference_graph_complete` means the retained graph can be traversed safely: Manifests and Image Configs pass Descriptor and parsing checks, while Layers are checked for regular-file presence and Descriptor size without a full content-digest scan. Same-size Layer corruption therefore does not make content reclaimable or permit its deletion.
