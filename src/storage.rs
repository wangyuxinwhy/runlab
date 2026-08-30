mod oci;
mod sqlite;

pub(crate) use oci::LocalOciStore;
#[cfg(target_os = "linux")]
pub(crate) use sqlite::StorageDatabaseFacts;
pub(crate) use sqlite::{
    Database, ExecutionOwner, ExecutionPhase, NewObservation, NewObservationRetraction,
    NewObservationType, NewRun, ObservationInsertion, ObservationRetractionInsertion,
    ObservationTypeInsertion, PlannedRunDeletion, RunCancellation, RunDeletionCommit,
    RunDeletionConflict, RunInsertion, RunTombstone, StoredObservation,
    StoredObservationRetraction, StoredObservationType, StoredRun, StoredRunDeletionFacts,
};
