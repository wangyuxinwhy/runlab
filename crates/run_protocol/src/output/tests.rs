use std::{collections::BTreeMap, num::NonZeroU64};

use chrono::{DateTime, FixedOffset};
use oci_spec::image::Descriptor;

use super::stdio::validate_stream_shape;
use super::*;
use crate::{
    ImageDescriptor, Network, OutputError, ProgramId, ProgramInput, RunInput, RuntimeConfig,
    Secrets,
};

fn at(second: u32) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(&format!("2026-08-25T12:00:{second:02}+08:00")).expect("timestamp")
}

fn image(byte: char) -> ImageDescriptor {
    let digest = byte.to_string().repeat(64);
    let descriptor: Descriptor = serde_json::from_value(serde_json::json!({
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "digest": format!("sha256:{digest}"),
        "size": 123
    }))
    .expect("OCI Descriptor");
    ImageDescriptor::new(descriptor).expect("Image Manifest")
}

fn runtime() -> RuntimeConfig {
    RuntimeConfig::parse(
        serde_json::to_vec(&serde_json::json!({
            "ociVersion": "1.3.0",
            "root": {"path": "rootfs"},
            "process": {
                "terminal": false,
                "args": ["/bin/true"],
                "cwd": "/",
                "user": {"uid": 0, "gid": 0}
            },
            "linux": {}
        }))
        .expect("runtime JSON"),
    )
    .expect("RuntimeConfig")
}

fn input(program_ids: &[&str]) -> RunInput {
    let programs = program_ids
        .iter()
        .map(|program_id| {
            (
                ProgramId::new(*program_id),
                ProgramInput::new(image('a'), runtime(), Vec::new(), Secrets::empty())
                    .expect("ProgramInput"),
            )
        })
        .collect();
    RunInput::new(programs, NonZeroU64::new(1000), Network::Isolated).expect("RunInput")
}

fn error(second: u32, stage: OperationStage, message: &str) -> OperationError {
    OperationError::new(at(second), stage, message, None).expect("OperationError")
}

fn simple_program() -> ProgramOutput {
    program_with_lifecycle(
        OperationReport::succeeded(CreateFacts::new(at(1))),
        OperationReport::succeeded(StartFacts::new(at(2))),
        ProcessResult::Exited {
            code: 0,
            ended_at: at(3),
        },
    )
    .expect("valid ProgramOutput")
}

fn program_with_lifecycle(
    create: OperationReport<CreateFacts>,
    start: OperationReport<StartFacts>,
    process: ProcessResult,
) -> Result<ProgramOutput, OutputError> {
    ProgramOutput::new(
        create,
        start,
        process,
        StdinOutput::new(
            OperationReport::succeeded(StdinWriteFacts::new(0)),
            OperationReport::succeeded(()),
        ),
        OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stdout")),
        OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stderr")),
        [],
        Availability::available(image('b')),
        [],
    )
}

#[test]
fn operation_report_constructors_keep_four_states_distinct() {
    let not_attempted = OperationReport::<CreateFacts>::not_attempted("dependency create failed")
        .expect("not attempted");
    assert_eq!(not_attempted.status(), OperationStatus::NotAttempted);
    assert!(not_attempted.facts().is_none());
    assert_eq!(not_attempted.errors().count(), 0);

    let succeeded = OperationReport::succeeded(CreateFacts::new(at(1)));
    assert_eq!(succeeded.status(), OperationStatus::Succeeded);
    assert!(succeeded.facts().is_some());

    let failed = OperationReport::<CreateFacts>::failed(
        error(2, OperationStage::Create, "runtime rejected create"),
        [],
    );
    assert_eq!(failed.status(), OperationStatus::Failed);
    assert!(failed.facts().is_none());
    assert_eq!(failed.errors().count(), 1);

    let unknown = OperationReport::<CreateFacts>::unknown(
        "runtime result was not observable",
        [error(2, OperationStage::Create, "inspect timed out")],
    )
    .expect("unknown");
    assert_eq!(unknown.status(), OperationStatus::Unknown);
    assert_eq!(unknown.errors().count(), 1);

    assert!(OperationReport::<()>::not_attempted(" ").is_err());
    assert!(OperationReport::<()>::unknown("", []).is_err());
}

#[test]
fn unavailable_and_unknown_require_reasons() {
    assert!(Availability::<()>::unavailable("").is_err());
    assert!(ProcessResult::never_started(" ").is_err());
    assert!(
        ProcessResult::unknown(
            "",
            Availability::unavailable("end time not observed").expect("unavailable"),
        )
        .is_err()
    );

    let never_started =
        ProcessResult::never_started("OCI start was not attempted").expect("known fact");
    let unknown = ProcessResult::unknown(
        "wait result unavailable",
        Availability::unavailable("end time unavailable").expect("unavailable"),
    )
    .expect("unknown fact");
    assert_ne!(never_started, unknown);
}

#[test]
fn stream_shape_enforces_the_fixed_limit_without_guessing_truncation() {
    assert!(validate_stream_shape(0, false).is_ok());
    assert!(validate_stream_shape(MAX_CAPTURED_STREAM_BYTES, false).is_ok());
    assert!(validate_stream_shape(MAX_CAPTURED_STREAM_BYTES, true).is_ok());
    assert!(validate_stream_shape(MAX_CAPTURED_STREAM_BYTES - 1, true).is_err());
    assert!(validate_stream_shape(MAX_CAPTURED_STREAM_BYTES + 1, false).is_err());

    let partial = StreamFacts::new(b"partial".to_vec(), false, false).expect("partial stream");
    let report = OperationReport::<StreamFacts>::failed_with_facts(
        partial,
        error(3, OperationStage::StdoutRead, "read failed"),
        [],
    );
    assert_eq!(report.facts().expect("partial facts").bytes(), b"partial");
}

#[test]
fn program_errors_are_aggregated_once_from_their_owning_operations() {
    let program = ProgramOutput::new(
        OperationReport::succeeded(CreateFacts::new(at(1))),
        OperationReport::unknown(
            "start result unavailable",
            [error(2, OperationStage::Start, "start")],
        )
        .expect("unknown start"),
        ProcessResult::unknown(
            "process result unavailable",
            Availability::unavailable("end time unavailable").expect("unavailable end"),
        )
        .expect("unknown process"),
        StdinOutput::new(
            OperationReport::<StdinWriteFacts>::failed_with_facts(
                StdinWriteFacts::new(2),
                error(2, OperationStage::StdinWrite, "stdin"),
                [],
            ),
            OperationReport::succeeded(()),
        ),
        OperationReport::<StreamFacts>::failed_with_facts(
            StreamFacts::new(b"x".to_vec(), false, false).expect("partial stdout"),
            error(3, OperationStage::StdoutRead, "stdout"),
            [],
        ),
        OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stderr")),
        [StopAction::new(
            StopSignal::Term,
            at(4),
            StopActionResult::Rejected(error(4, OperationStage::Signal, "signal")),
        )],
        Availability::unavailable("rootfs never became stable")
            .expect("unavailable final environment"),
        [error(5, OperationStage::Cleanup, "cleanup")],
    )
    .expect("valid ProgramOutput");

    let messages = program
        .errors()
        .map(OperationError::message)
        .collect::<Vec<_>>();
    assert_eq!(messages, ["start", "stdin", "stdout", "signal", "cleanup"]);
}

#[test]
fn output_program_keys_must_exactly_match_input() {
    let input = input(&["dependency", "primary"]);
    let execution =
        ExecutionOutput::new(ExecutionInterval::entered(at(1), at(3)), false, false, [])
            .expect("execution");

    let mut missing = BTreeMap::new();
    missing.insert(ProgramId::primary(), simple_program());
    assert!(RunOutput::new(&input, execution.clone(), missing).is_err());

    let mut exact = BTreeMap::new();
    exact.insert(ProgramId::new("dependency"), simple_program());
    exact.insert(ProgramId::primary(), simple_program());
    RunOutput::new(&input, execution, exact).expect("matching output");
}

#[test]
fn execution_times_and_termination_cause_are_coherent() {
    let not_entered =
        ExecutionInterval::not_entered("cancelled before start").expect("not-entered interval");
    ExecutionOutput::new(not_entered.clone(), false, true, []).expect("cancelled before start");

    ExecutionOutput::new(ExecutionInterval::entered(at(2), at(1)), false, false, [])
        .expect("wall clock observations may move backwards");
    assert!(ExecutionOutput::new(not_entered, true, false, []).is_err());
    assert!(
        ExecutionOutput::new(ExecutionInterval::entered(at(1), at(2)), true, true, [],).is_err()
    );
}

#[test]
fn primary_cannot_start_until_every_dependency_started_successfully() {
    let cases = [
        program_with_lifecycle(
            OperationReport::failed(error(1, OperationStage::Create, "create failed"), []),
            OperationReport::not_attempted("create failed").expect("not attempted"),
            ProcessResult::never_started("start was not attempted").expect("never started"),
        )
        .expect("not-attempted dependency"),
        program_with_lifecycle(
            OperationReport::succeeded(CreateFacts::new(at(1))),
            OperationReport::failed(error(2, OperationStage::Start, "start failed"), []),
            ProcessResult::never_started("start failed").expect("never started"),
        )
        .expect("failed dependency"),
        program_with_lifecycle(
            OperationReport::succeeded(CreateFacts::new(at(1))),
            OperationReport::unknown(
                "start result unavailable",
                [error(2, OperationStage::Start, "start unavailable")],
            )
            .expect("unknown start"),
            ProcessResult::unknown(
                "process state unavailable",
                Availability::unavailable("end unavailable").expect("unavailable end"),
            )
            .expect("unknown process"),
        )
        .expect("unknown dependency"),
    ];

    for dependency in cases {
        let input = input(&["dependency", "primary"]);
        let execution =
            ExecutionOutput::new(ExecutionInterval::entered(at(1), at(3)), false, false, [])
                .expect("execution");
        let programs = BTreeMap::from([
            (ProgramId::new("dependency"), dependency),
            (ProgramId::primary(), simple_program()),
        ]);

        let error = RunOutput::new(&input, execution, programs)
            .expect_err("primary start must be rejected");
        assert_eq!(error.path(), "programs[\"primary\"].start");
    }
}

#[test]
fn program_constructor_rejects_stream_success_without_eof() {
    let error = ProgramOutput::new(
        OperationReport::succeeded(CreateFacts::new(at(1))),
        OperationReport::succeeded(StartFacts::new(at(2))),
        ProcessResult::Exited {
            code: 0,
            ended_at: at(3),
        },
        StdinOutput::new(
            OperationReport::succeeded(StdinWriteFacts::new(0)),
            OperationReport::succeeded(()),
        ),
        OperationReport::succeeded(
            StreamFacts::new(b"partial".to_vec(), false, false).expect("stream facts"),
        ),
        OperationReport::succeeded(StreamFacts::new(Vec::new(), false, true).expect("stderr")),
        [],
        Availability::available(image('b')),
        [],
    )
    .expect_err("success requires EOF");

    assert_eq!(error.path(), "program.stdout");
}

#[test]
fn program_constructor_enforces_error_ownership_without_value_deduplication() {
    let misplaced = program_with_lifecycle(
        OperationReport::failed(error(1, OperationStage::StderrRead, "wrong owner"), []),
        OperationReport::not_attempted("create failed").expect("not attempted"),
        ProcessResult::never_started("start was not attempted").expect("never started"),
    )
    .expect_err("create cannot own stderr errors");
    assert_eq!(misplaced.path(), "program.create");

    let duplicate = error(4, OperationStage::Cleanup, "cleanup failed");
    let base = simple_program();
    let repeated_observations = ProgramOutput::new(
        base.create().clone(),
        base.start().clone(),
        base.process().clone(),
        base.stdin().clone(),
        base.stdout().clone(),
        base.stderr().clone(),
        [],
        base.final_environment().clone(),
        [duplicate.clone(), duplicate],
    )
    .expect("equal values may represent separate observations");
    assert_eq!(repeated_observations.errors().count(), 2);
}

#[test]
fn failed_or_unknown_start_does_not_override_later_process_evidence() {
    for start in [
        OperationReport::failed(error(2, OperationStage::Start, "runtime failed"), []),
        OperationReport::unknown(
            "runtime result unavailable",
            [error(2, OperationStage::Start, "runtime unavailable")],
        )
        .expect("unknown start"),
    ] {
        program_with_lifecycle(
            OperationReport::succeeded(CreateFacts::new(at(1))),
            start,
            ProcessResult::Exited {
                code: 17,
                ended_at: at(3),
            },
        )
        .expect("direct process evidence survives start-operation uncertainty");
    }
}

#[test]
fn bounded_stop_never_starts_with_kill() {
    let base = simple_program();
    let error = ProgramOutput::new(
        base.create().clone(),
        base.start().clone(),
        base.process().clone(),
        base.stdin().clone(),
        base.stdout().clone(),
        base.stderr().clone(),
        [StopAction::new(
            StopSignal::Kill,
            at(4),
            StopActionResult::Accepted,
        )],
        base.final_environment().clone(),
        [],
    )
    .expect_err("TERM must be attempted first");

    assert_eq!(error.path(), "program.stop_actions");
}

#[test]
fn execution_scope_rejects_program_operation_errors() {
    let error = ExecutionOutput::new(
        ExecutionInterval::entered(at(1), at(2)),
        false,
        false,
        [error(1, OperationStage::Create, "misplaced create error")],
    )
    .expect_err("create error belongs to a Program");

    assert_eq!(error.path(), "execution.errors");
}

#[test]
fn process_supervision_error_belongs_to_the_program() {
    let base = simple_program();
    ProgramOutput::new(
        base.create().clone(),
        base.start().clone(),
        base.process().clone(),
        base.stdin().clone(),
        base.stdout().clone(),
        base.stderr().clone(),
        [],
        base.final_environment().clone(),
        [error(
            1,
            OperationStage::ProcessSupervision,
            "created process identity unavailable",
        )],
    )
    .expect("process supervision is an additional Program-scoped operation");
}
