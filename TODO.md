# TODO

## Make `runlab run list` a bounded Agent selection view

### Pointer

01a03823-4d41-75c3-9311-9e13877547f4

### Conclusion

The current default page of 20 Run summaries is still too large in real Agent use, and `--limit` plus UUID pagination does not help an Agent locate the relevant subset. `run list` is a selection view, not the complete Run fact surface: it must remain bounded, add only filters demonstrated by real lookup workflows, and present timestamps at a precision useful for selection while `run get` retains the original exact timestamps. When `run start` selects an Initial Image by Catalog name, RunLab must preserve that caller-visible name as an accepted product fact; `run list` must not try to reconstruct it later from the mutable Catalog. This change belongs entirely to RunLab and must not add fields to Run Protocol or Run Engine.

### Tasks

- [ ] `src/cli/run.rs`: refine the default list bound, add the validated filter arguments, document them completely in CLI help, and forward them on macOS.
- [ ] `src/run.rs`: preserve the selected Initial Image name at acceptance and project the bounded, filtered summary with selection-oriented timestamp precision.
- [ ] `src/storage/sqlite.rs`: migrate RunLab persistence for the accepted Initial Image name and implement indexed filtering and continuation without changing complete Run facts.
- [ ] `src/managed_vm/transport.rs`: forward the final list query contract to the Managed VM without an arbitrary argv path.
- [ ] `tests/cli.rs` and `tests/cli_macos.rs`: cover bounds, continuation, each validated filter, timestamp projection, immutable Initial Image name, help text, and macOS forwarding as separate-process CLI behavior.
