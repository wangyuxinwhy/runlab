//! Linux OCI execution through a caller-selected `runc` executable.

use std::path::PathBuf;
use std::sync::Arc;

use run_protocol::{EngineError, RunInput, RunOutput};

use crate::{
    CancellationToken, EngineEventSink, OciContentStore, OperationTimeouts, RunEngine,
    live_event::IgnoreEventSink,
};

mod budget;
mod capture;
mod cgroup;
mod cleanup;
mod container_path;
mod execution;
mod linux_evidence;
mod network;
mod prepare;
mod profile;
mod program;
mod report;
mod runc;
mod start;
mod stdio;
mod stop;
mod subprocess;
mod time;
mod wait;

use budget::{BudgetedStore, OperationBudget};
use execution::{ExecutionContext, execute};
use prepare::prepare;
use subprocess::InvocationSupervisor;

/// Linux reference implementation backed directly by an OCI runtime.
pub struct NativeEngine {
    store: Arc<dyn OciContentStore>,
    workspace_root: PathBuf,
    runc_executable: PathBuf,
    timeouts: OperationTimeouts,
}

impl NativeEngine {
    /// Constructs an Engine with fixed invocation-independent resource paths and deadlines.
    #[must_use]
    pub fn new(
        store: Arc<dyn OciContentStore>,
        workspace_root: impl Into<PathBuf>,
        runc_executable: impl Into<PathBuf>,
        timeouts: OperationTimeouts,
    ) -> Self {
        Self {
            store,
            workspace_root: workspace_root.into(),
            runc_executable: runc_executable.into(),
            timeouts,
        }
    }

    /// Returns the fixed finite deadlines used for Engine-owned operations.
    #[must_use]
    pub fn operation_timeouts(&self) -> OperationTimeouts {
        self.timeouts
    }

    fn execution_context(&self, event_sink: Arc<dyn EngineEventSink>) -> ExecutionContext {
        ExecutionContext::new(Arc::clone(&self.store), self.timeouts, event_sink)
    }

    fn run_supervised(
        &self,
        input: &RunInput,
        cancellation: &CancellationToken,
        supervisor: &InvocationSupervisor,
        event_sink: Arc<dyn EngineEventSink>,
    ) -> Result<RunOutput, EngineError> {
        let budget = OperationBudget::new(self.timeouts.preparation(), "NativeEngine preparation")
            .map_err(|error| EngineError::internal(format!("{error:#}")))?;
        let store = BudgetedStore::new(Arc::clone(&self.store), budget);
        let preflight = prepare(
            input,
            &store,
            budget,
            supervisor,
            &self.workspace_root,
            &self.runc_executable,
        );
        let mut prepared = match preflight {
            Ok(prepared) => prepared,
            Err(input_error) => match supervisor.finalize(budget.deadline()) {
                Ok(()) => return Err(input_error),
                Err(supervision_error) => {
                    return Err(EngineError::internal(format!(
                        "preflight failed ({input_error}) and supervisor cleanup did not reach Reaped before the preparation deadline: {supervision_error:#}"
                    )));
                }
            },
        };
        execute(
            &self.execution_context(event_sink),
            input,
            cancellation,
            &mut prepared,
        )
    }

    /// Executes one invocation while reporting best-effort Live Events.
    ///
    /// The event sink is invocation-scoped and does not affect the returned Run Protocol result.
    ///
    /// # Errors
    ///
    /// Returns the same [`EngineError`] categories as [`RunEngine::run`].
    pub fn run_with_events(
        &self,
        input: &RunInput,
        cancellation: &CancellationToken,
        event_sink: Arc<dyn EngineEventSink>,
    ) -> Result<RunOutput, EngineError> {
        let supervisor = InvocationSupervisor::new();
        self.run_supervised(input, cancellation, &supervisor, event_sink)
    }
}

impl RunEngine for NativeEngine {
    fn run(
        &self,
        input: RunInput,
        cancellation: CancellationToken,
    ) -> Result<RunOutput, EngineError> {
        self.run_with_events(&input, &cancellation, Arc::new(IgnoreEventSink))
    }
}
