use std::collections::BTreeMap;
use std::thread;
use std::time::Instant;

use run_protocol::{Availability, OperationStage, ProcessResult, ProgramId};
use serde::Deserialize;

use super::linux_evidence::{ProcExitMonitor, RawProcessResult};
use super::program::{ProgramRun, unattempted_stdin, unattempted_stream};
use super::report::operation_error;
use super::runc::{helper_message, runc_command};
use super::stdio::InputTransfer;
use super::subprocess::{RunningHelper, terminate_child};
use super::time::{POLL_INTERVAL, checked_deadline, wall_clock_now};
use crate::OperationTimeouts;

#[derive(Deserialize)]
struct RuncState {
    status: String,
}

#[cfg(test)]
pub(super) fn process_from_raw_wait_status(status: u32) -> ProcessResult {
    process_from_raw_result(RawProcessResult::from_wait_status(status))
}

fn process_from_raw_result(result: RawProcessResult) -> ProcessResult {
    match result {
        RawProcessResult::Exited(code) => ProcessResult::Exited {
            code,
            ended_at: wall_clock_now(),
        },
        RawProcessResult::Signaled(signal) => ProcessResult::Signaled {
            signal,
            ended_at: wall_clock_now(),
        },
        RawProcessResult::Unknown(status) => ProcessResult::unknown(
            format!("proc connector reported unsupported raw wait status 0x{status:x}"),
            Availability::available(wall_clock_now()),
        )
        .expect("non-empty reason"),
    }
}

pub(super) fn poll_children(runs: &mut BTreeMap<ProgramId, ProgramRun>) -> bool {
    let mut failed = false;
    for run in runs.values_mut() {
        if run.runtime.poll_failed {
            failed = true;
            continue;
        }
        if let Err(error) = poll_one(run) {
            run.runtime.poll_failed = true;
            failed = true;
            run.facts.errors.push(operation_error(
                OperationStage::Wait,
                format!("failed to poll runc supervisor: {error}"),
                error.raw_os_error().map(i64::from),
            ));
        }
    }
    failed
}

pub(super) fn poll_one(run: &mut ProgramRun) -> std::io::Result<bool> {
    run.pump_io();
    if run.facts.process.is_some() {
        return Ok(false);
    }
    if let Some(observed) = poll_raw_exit_monitor(run) {
        return Ok(observed);
    }
    if run
        .runtime
        .stopped_observation
        .is_some_and(|(_, deadline)| Instant::now() >= deadline)
    {
        finalize_stopped_observation(run);
        return Ok(true);
    }
    if run.runtime.stopped_observation.is_some() {
        return Ok(false);
    }
    if run.supervision.state_probe_failed {
        return Ok(false);
    }
    let Some(wait_timeout) = run
        .runtime
        .coordinates
        .as_ref()
        .map(|(_, _, _, wait_timeout)| *wait_timeout)
    else {
        return Ok(false);
    };
    if run.supervision.state_probe.is_none() {
        start_state_probe(run)?;
        return Ok(false);
    }
    if state_probe_expired(run) {
        return terminate_expired_state_probe(run);
    }
    finish_state_probe(run, wait_timeout)
}

fn poll_raw_exit_monitor(run: &mut ProgramRun) -> Option<bool> {
    let result = run.runtime.exit_monitor.as_mut()?.try_result();
    match result {
        Ok(Some(result)) => {
            observe_raw_exit(run, result);
            Some(true)
        }
        Ok(None) => None,
        Err(error) => monitor_failed(run, &error),
    }
}

fn observe_raw_exit(run: &mut ProgramRun, result: RawProcessResult) {
    let unsubscribe = run
        .runtime
        .exit_monitor
        .as_mut()
        .expect("monitor was polled")
        .unsubscribe();
    run.observe_raw_process_result(process_from_raw_result(result));
    run.runtime.exit_monitor = None;
    let state_probe_stop = run
        .supervision
        .state_probe
        .as_mut()
        .map(RunningHelper::terminate);
    if state_probe_stop.as_ref().is_none_or(Result::is_ok) {
        run.supervision.state_probe = None;
        run.supervision.state_probe_deadline = None;
    }
    if let Err(error) = unsubscribe {
        run.facts.errors.push(operation_error(
            OperationStage::Wait,
            format!("failed to unsubscribe proc connector after exit: {error:#}"),
            None,
        ));
    }
    if let Some(Err(error)) = state_probe_stop {
        run.supervision.unreaped = true;
        run.facts.errors.push(operation_error(
            OperationStage::Wait,
            format!(
                "raw exit was proved but the concurrent runc state probe was not reaped: {error:#}"
            ),
            None,
        ));
    }
}

fn monitor_failed(run: &mut ProgramRun, error: &anyhow::Error) -> Option<bool> {
    let unsubscribe = run
        .runtime
        .exit_monitor
        .as_mut()
        .expect("monitor was polled")
        .unsubscribe();
    run.facts.errors.push(operation_error(
        OperationStage::Wait,
        format!("raw process-result monitoring failed; termination will remain Unknown: {error:#}"),
        None,
    ));
    if let Err(error) = unsubscribe {
        run.facts.errors.push(operation_error(
            OperationStage::Wait,
            format!("failed to unsubscribe invalid proc connector monitor: {error:#}"),
            None,
        ));
    }
    run.runtime.exit_monitor = None;
    if run.runtime.stopped_observation.is_some() {
        run.fallback_after_stopped(
            "runc state proved process termination, but raw exit monitoring failed",
        );
        return Some(true);
    }
    None
}

fn finalize_stopped_observation(run: &mut ProgramRun) {
    let unsubscribe = run
        .runtime
        .exit_monitor
        .as_mut()
        .map(ProcExitMonitor::unsubscribe);
    run.runtime.exit_monitor = None;
    run.facts.errors.push(operation_error(
        OperationStage::Wait,
        "raw proc exit event did not arrive before the process wait deadline",
        None,
    ));
    if let Some(Err(error)) = unsubscribe {
        run.facts.errors.push(operation_error(
            OperationStage::Wait,
            format!("failed to unsubscribe proc connector after wait deadline: {error:#}"),
            None,
        ));
    }
    run.fallback_after_stopped(
        "runc state proved process termination, but no raw exit event arrived before the wait deadline",
    );
}

fn start_state_probe(run: &mut ProgramRun) -> std::io::Result<()> {
    let (runc, root, runtime_id, wait_timeout) = run
        .runtime
        .coordinates
        .as_ref()
        .expect("caller checked runtime coordinates");
    let mut command = runc_command(runc, root);
    command.arg("state").arg(runtime_id);
    run.supervision.state_probe = Some(
        RunningHelper::spawn(&run.supervision.owner, &mut command)
            .map_err(std::io::Error::other)?,
    );
    run.supervision.state_probe_deadline = Some(
        checked_deadline(Instant::now(), *wait_timeout, "runc state probe deadline")
            .map_err(std::io::Error::other)?,
    );
    Ok(())
}

fn state_probe_expired(run: &ProgramRun) -> bool {
    run.supervision
        .state_probe_deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
}

fn terminate_expired_state_probe(run: &mut ProgramRun) -> std::io::Result<bool> {
    let termination = run
        .supervision
        .state_probe
        .as_mut()
        .expect("matched some")
        .terminate();
    if termination.is_ok() {
        run.supervision.state_probe = None;
    } else {
        run.supervision.unreaped = true;
    }
    run.supervision.state_probe_deadline = None;
    run.supervision.state_probe_failed = true;
    termination.map_err(std::io::Error::other)?;
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "runc state probe deadline exceeded",
    ))
}

fn finish_state_probe(
    run: &mut ProgramRun,
    wait_timeout: std::time::Duration,
) -> std::io::Result<bool> {
    let output = match run
        .supervision
        .state_probe
        .as_mut()
        .expect("matched some")
        .try_finish()
    {
        Ok(output) => output,
        Err(error) => {
            let termination = run
                .supervision
                .state_probe
                .as_mut()
                .expect("matched some")
                .terminate();
            if termination.is_ok() {
                run.supervision.state_probe = None;
            } else {
                run.supervision.unreaped = true;
            }
            run.supervision.state_probe_deadline = None;
            run.supervision.state_probe_failed = true;
            termination.map_err(std::io::Error::other)?;
            return Err(std::io::Error::other(format!(
                "runc state probe supervision failed: {error:#}"
            )));
        }
    };
    if let Some(output) = output {
        run.supervision.state_probe = None;
        run.supervision.state_probe_deadline = None;
        if !output.status.success() {
            run.supervision.state_probe_failed = true;
            return Err(std::io::Error::other(helper_message("runc state", &output)));
        }
        let state: RuncState = match serde_json::from_slice(&output.stdout) {
            Ok(state) => state,
            Err(error) => {
                run.supervision.state_probe_failed = true;
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
        };
        if state.status == "stopped" {
            run.observe_runtime_stopped(wait_timeout);
            return Ok(run.facts.process.is_some());
        }
    }
    Ok(false)
}

pub(super) fn finalize_children(
    runs: &mut BTreeMap<ProgramId, ProgramRun>,
    timeouts: OperationTimeouts,
) {
    let wait_deadline = checked_deadline(Instant::now(), timeouts.wait(), "process wait deadline")
        .expect("validated OperationTimeouts fit Instant");
    wait_for_process_results(runs, wait_deadline);
    if mark_forced_confirmation(runs) {
        let confirmation_deadline = checked_deadline(
            Instant::now(),
            timeouts.forced_stop_confirmation(),
            "forced-stop confirmation deadline",
        )
        .expect("validated OperationTimeouts fit Instant");
        wait_for_process_results(runs, confirmation_deadline);
    }
    for run in runs.values_mut() {
        finalize_process_evidence(run, timeouts);
    }
    drain_streams(runs, timeouts);
}

fn wait_for_process_results(runs: &mut BTreeMap<ProgramId, ProgramRun>, deadline: Instant) {
    loop {
        poll_children(runs);
        if all_process_results_observed(runs) || Instant::now() >= deadline {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn all_process_results_observed(runs: &BTreeMap<ProgramId, ProgramRun>) -> bool {
    runs.values()
        .all(|run| run.runtime.coordinates.is_none() || run.facts.process.is_some())
}

fn mark_forced_confirmation(runs: &mut BTreeMap<ProgramId, ProgramRun>) -> bool {
    let mut needed = false;
    for run in runs.values_mut() {
        if run.runtime.coordinates.is_some() && run.facts.process.is_none() {
            needed = true;
            run.facts.errors.push(operation_error(
                OperationStage::Wait,
                "process wait deadline exceeded; entering forced-stop confirmation",
                None,
            ));
        }
    }
    needed
}

fn finalize_process_evidence(run: &mut ProgramRun, timeouts: OperationTimeouts) {
    unsubscribe_exit_monitor(run);
    terminate_create_supervisor(run, timeouts);
    terminate_state_probe(run);
    if run.runtime.coordinates.is_some() && run.facts.process.is_none() {
        record_unconfirmed_process(run);
    }
}

fn unsubscribe_exit_monitor(run: &mut ProgramRun) {
    if let Some(mut monitor) = run.runtime.exit_monitor.take()
        && let Err(error) = monitor.unsubscribe()
    {
        run.facts.errors.push(operation_error(
            OperationStage::Wait,
            format!("failed to unsubscribe proc connector during finalization: {error:#}"),
            None,
        ));
    }
}

fn terminate_create_supervisor(run: &mut ProgramRun, timeouts: OperationTimeouts) {
    if run.supervision.create_child.is_some()
        && let Err(error) = terminate_child(
            &run.supervision.owner,
            &mut run.supervision.create_child,
            timeouts.forced_stop_confirmation(),
        )
    {
        run.supervision.unreaped = true;
        run.facts.errors.push(operation_error(
            OperationStage::Wait,
            format!("failed to terminate and reap runc create supervisor: {error:#}"),
            None,
        ));
    }
}

fn terminate_state_probe(run: &mut ProgramRun) {
    if let Some(helper) = &mut run.supervision.state_probe {
        match helper.terminate() {
            Ok(()) => {
                run.supervision.state_probe = None;
                run.supervision.state_probe_deadline = None;
            }
            Err(error) => run.facts.errors.push(operation_error(
                OperationStage::Wait,
                format!("failed to terminate and reap runc state probe: {error:#}"),
                None,
            )),
        }
    }
    run.supervision.unreaped |= run
        .supervision
        .state_probe
        .as_ref()
        .is_some_and(|helper| !helper.is_reaped());
}

fn record_unconfirmed_process(run: &mut ProgramRun) {
    let termination = run
        .supervision
        .state_probe
        .as_mut()
        .map(RunningHelper::terminate);
    if termination.as_ref().is_none_or(Result::is_ok) {
        run.supervision.state_probe = None;
    }
    run.facts.process = Some(
        ProcessResult::unknown(
            "the container did not reach a directly observed stopped state before the forced-stop confirmation deadline",
            Availability::unavailable("no process-end observation was obtained")
                .expect("literal reason"),
        )
        .expect("literal reason"),
    );
    run.runtime.writer_stopped = false;
    run.facts.errors.push(operation_error(
        OperationStage::Wait,
        termination.and_then(Result::err).map_or_else(
            || "forced-stop confirmation deadline exceeded".to_owned(),
            |error| format!("forced-stop confirmation failed: {error:#}"),
        ),
        None,
    ));
}

fn drain_streams(runs: &mut BTreeMap<ProgramId, ProgramRun>, timeouts: OperationTimeouts) {
    let drain_deadline = checked_deadline(
        Instant::now(),
        timeouts.stream_drain(),
        "stream drain deadline",
    )
    .expect("validated OperationTimeouts fit Instant");
    loop {
        for run in runs.values_mut() {
            run.pump_output();
        }
        if runs.values().all(ProgramRun::io_complete) || Instant::now() >= drain_deadline {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    for run in runs.values_mut() {
        finish_streams(run);
    }
}

fn finish_streams(run: &mut ProgramRun) {
    run.facts.stdin = Some(
        run.io
            .stdin_transfer
            .take()
            .map_or_else(unattempted_stdin, InputTransfer::finish),
    );
    run.facts.stdout = Some(
        run.io
            .stdout_drain
            .take()
            .map_or_else(unattempted_stream, |drain| {
                drain.finish(OperationStage::StdoutRead)
            }),
    );
    run.facts.stderr = Some(
        run.io
            .stderr_drain
            .take()
            .map_or_else(unattempted_stream, |drain| {
                drain.finish(OperationStage::StderrRead)
            }),
    );
}
