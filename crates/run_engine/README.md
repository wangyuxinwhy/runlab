# run_engine

`run_engine` implements RunLab's synchronous `RunEngine` interface. Its reference implementation is the Linux `NativeEngine`, backed by standard OCI objects and `runc`.

The crate does not assign Run identities, publish persistent Run Records, resolve Catalog names, or interpret execution results as judgments. See the [Engine implementation contract](https://wangyuxinwhy.github.io/runlab/design/generated/engine-contract) for the responsibility boundary.
