# run_protocol

`run_protocol` defines RunLab's pure execution protocol: `RunInput`, `RunEngine::run`, `RunOutput`, and `EngineError`.

The crate owns data structures, validation, and protocol invariants. It does not own Run identities, persistence, catalogs, recovery, or execution mechanisms. See the [Run Protocol documentation](https://wangyuxinwhy.github.io/runlab/design/generated/run-protocol) for the complete contract.
