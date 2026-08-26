use std::collections::BTreeMap;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result as AnyResult;
use chrono::{DateTime, FixedOffset};
use run_protocol::{
    OperationStage, OperationStatus, ProgramId, StopAction, StopActionResult, StopSignal,
};

use super::prepare::PreparedProgram;
use super::program::ProgramRun;
use super::report::operation_error;
use super::runc::{helper_message, runc_command};
use super::subprocess::{HelperOutput, RunningHelper, SUPERVISOR_REAP_LIMIT};
use super::time::{POLL_INTERVAL, checked_deadline, wall_clock_now};
use super::wait::poll_children;
use crate::{OperationTimeouts, STOP_GRACE_PERIOD};

pub(super) fn stop_all(
    runc: &Path,
    root: &Path,
    programs: &BTreeMap<ProgramId, PreparedProgram>,
    outcomes: &mut BTreeMap<ProgramId, ProgramRun>,
    timeouts: OperationTimeouts,
) {
    let ids = outcomes
        .iter()
        .filter(|(_, run)| {
            matches!(
                run.facts.start.status(),
                OperationStatus::Succeeded | OperationStatus::Unknown
            ) && run.facts.process.is_none()
                && !run.runtime.writer_stopped
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return;
    }
    let first_term = Instant::now();
    let grace = checked_deadline(first_term, STOP_GRACE_PERIOD, "shared stop grace")
        .expect("fixed grace fits Instant");
    let term_helper_cap = grace
        .checked_sub(Duration::from_secs(2))
        .expect("shared grace exceeds bounded helper reap reserve");
    signal_all(SignalPhase {
        runc,
        root,
        programs,
        outcomes,
        ids: &ids,
        signal: StopSignal::Term,
        timeout: timeouts.term_signal(),
        absolute_cap: Some(term_helper_cap),
    });
    while Instant::now() < grace {
        poll_children(outcomes);
        if ids.iter().all(|id| outcomes[id].facts.process.is_some()) {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let remaining = ids
        .into_iter()
        .filter(|id| outcomes[id].facts.process.is_none())
        .collect::<Vec<_>>();
    signal_all(SignalPhase {
        runc,
        root,
        programs,
        outcomes,
        ids: &remaining,
        signal: StopSignal::Kill,
        timeout: timeouts.kill_signal(),
        absolute_cap: None,
    });
}

struct SignalPhase<'a> {
    runc: &'a Path,
    root: &'a Path,
    programs: &'a BTreeMap<ProgramId, PreparedProgram>,
    outcomes: &'a mut BTreeMap<ProgramId, ProgramRun>,
    ids: &'a [ProgramId],
    signal: StopSignal,
    timeout: Duration,
    absolute_cap: Option<Instant>,
}

fn signal_all(mut phase: SignalPhase<'_>) {
    phase.run();
}

impl SignalPhase<'_> {
    fn run(&mut self) {
        let signal_text = self.signal_text();
        let phase_deadline = self.deadline();
        let mut attempts = self.spawn_attempts(signal_text);
        self.poll_attempts(&mut attempts, phase_deadline);
        request_attempt_termination(&mut attempts);
        self.reap_attempts(&mut attempts);
        self.record_attempts(attempts, signal_text);
    }

    fn signal_text(&self) -> &'static str {
        match self.signal {
            StopSignal::Term => "TERM",
            StopSignal::Kill => "KILL",
        }
    }

    fn deadline(&self) -> Instant {
        let started = Instant::now();
        let deadline = checked_deadline(started, self.timeout, "runc signal deadline")
            .expect("validated OperationTimeouts fit Instant");
        self.absolute_cap.map_or(deadline, |cap| cap.min(deadline))
    }

    fn spawn_attempts(&self, signal_text: &str) -> Vec<TermAttempt> {
        let mut attempts = Vec::with_capacity(self.ids.len());
        for id in self.ids {
            let attempted_at = wall_clock_now();
            let mut command = runc_command(self.runc, self.root);
            command
                .arg("kill")
                .arg("--all")
                .arg(&self.programs[id].runtime_id)
                .arg(signal_text);
            let (helper, spawn_error) =
                match RunningHelper::spawn(&self.outcomes[id].supervision.owner, &mut command) {
                    Ok(helper) => (Some(helper), None),
                    Err(error) => (None, Some(format!("{error:#}"))),
                };
            attempts.push(TermAttempt {
                id: id.clone(),
                attempted_at,
                helper,
                spawn_error,
                output: None,
            });
        }
        attempts
    }

    fn poll_attempts(&mut self, attempts: &mut [TermAttempt], deadline: Instant) {
        while Instant::now() < deadline
            && attempts
                .iter()
                .any(|attempt| attempt.helper.is_some() && attempt.output.is_none())
        {
            for attempt in &mut *attempts {
                if let Some(helper) = &mut attempt.helper {
                    match helper.try_finish() {
                        Ok(Some(output)) => attempt.output = Some(Ok(output)),
                        Ok(None) => {}
                        Err(error) => attempt.output = Some(Err(error)),
                    }
                }
            }
            poll_children(self.outcomes);
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn reap_attempts(&mut self, attempts: &mut [TermAttempt]) {
        let reap_deadline = checked_deadline(
            Instant::now(),
            SUPERVISOR_REAP_LIMIT,
            "signal helper reap deadline",
        )
        .expect("fixed helper reap limit fits Instant");
        while Instant::now() < reap_deadline
            && attempts.iter().any(|attempt| {
                attempt
                    .helper
                    .as_ref()
                    .is_some_and(|helper| !helper.is_reaped())
            })
        {
            for attempt in &mut *attempts {
                if let Some(helper) = &mut attempt.helper
                    && !helper.is_reaped()
                    && let Err(error) = helper.poll_reaped()
                {
                    attempt.output = Some(Err(error));
                }
            }
            poll_children(self.outcomes);
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn record_attempts(&mut self, attempts: Vec<TermAttempt>, signal_text: &str) {
        for attempt in attempts {
            let unreaped = attempt
                .helper
                .as_ref()
                .is_some_and(|helper| !helper.is_reaped());
            let result = stop_result(&attempt, signal_text, unreaped);
            let outcome = self.outcomes.get_mut(&attempt.id).expect("output slot");
            outcome.supervision.unreaped |= unreaped;
            outcome.facts.stop_actions.push(StopAction::new(
                self.signal,
                attempt.attempted_at,
                result,
            ));
        }
    }
}

fn request_attempt_termination(attempts: &mut [TermAttempt]) {
    for attempt in attempts {
        if let Some(helper) = &mut attempt.helper
            && !helper.is_reaped()
            && let Err(error) = helper.request_terminate()
        {
            attempt.output = Some(Err(error));
        }
    }
}

fn stop_result(attempt: &TermAttempt, signal_text: &str, unreaped: bool) -> StopActionResult {
    if unreaped {
        return StopActionResult::unknown(
            format!("runc kill {signal_text} helper was not reaped before its bounded confirmation deadline"),
            [],
        )
        .expect("non-empty reason");
    }
    match attempt.output.as_ref() {
        Some(Ok(output)) if output.status.success() => StopActionResult::Accepted,
        Some(Ok(output)) => StopActionResult::Rejected(operation_error(
            OperationStage::Signal,
            helper_message(&format!("runc kill {signal_text}"), output),
            output.status.code().map(i64::from),
        )),
        Some(Err(error)) => {
            StopActionResult::unknown(format!("runc kill {signal_text}: {error:#}"), [])
                .expect("non-empty reason")
        }
        None if attempt.spawn_error.is_some() => StopActionResult::unknown(
            format!(
                "failed to spawn runc kill {signal_text}: {}",
                attempt.spawn_error.as_deref().expect("matched some")
            ),
            [],
        )
        .expect("non-empty reason"),
        None => StopActionResult::unknown(
            format!("runc kill {signal_text} did not report before its deadline"),
            [],
        )
        .expect("literal reason"),
    }
}

struct TermAttempt {
    id: ProgramId,
    attempted_at: DateTime<FixedOffset>,
    helper: Option<RunningHelper>,
    spawn_error: Option<String>,
    output: Option<AnyResult<HelperOutput>>,
}
