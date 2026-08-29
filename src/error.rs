use std::fmt;
use std::io::Write as _;

use anyhow::Error;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCategory {
    InvalidInput,
    NotFound,
    Conflict,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ErrorEnvelope {
    schema_version: u32,
    kind: String,
    category: ErrorCategory,
    stage: String,
    message: String,
    run_id: Option<String>,
    accepted: Option<bool>,
    run_created: Option<bool>,
    retryable: bool,
    recovery: Option<String>,
}

#[derive(Debug)]
struct ClassifiedError {
    source: Error,
    category: ErrorCategory,
    stage: String,
    run_id: Option<String>,
    accepted: Option<bool>,
    run_created: Option<bool>,
    retryable: bool,
    recovery: Option<String>,
}

#[derive(Debug)]
pub(crate) struct RemoteError {
    pub(crate) envelope: ErrorEnvelope,
    pub(crate) already_emitted: bool,
}

impl fmt::Display for ClassifiedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for ClassifiedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.envelope.message)
    }
}

impl std::error::Error for RemoteError {}

pub(crate) struct ErrorFacts {
    pub(crate) category: ErrorCategory,
    pub(crate) stage: &'static str,
    pub(crate) run_id: Option<String>,
    pub(crate) accepted: Option<bool>,
    pub(crate) run_created: Option<bool>,
    pub(crate) retryable: bool,
    pub(crate) recovery: Option<String>,
}

impl ErrorFacts {
    pub(crate) fn before_run(category: ErrorCategory, stage: &'static str) -> Self {
        Self {
            category,
            stage,
            run_id: None,
            accepted: Some(false),
            run_created: Some(false),
            retryable: false,
            recovery: None,
        }
    }
}

pub(crate) fn classify(source: Error, facts: ErrorFacts) -> Error {
    Error::new(ClassifiedError {
        source,
        category: facts.category,
        stage: facts.stage.to_owned(),
        run_id: facts.run_id,
        accepted: facts.accepted,
        run_created: facts.run_created,
        retryable: facts.retryable,
        recovery: facts.recovery,
    })
}

pub(crate) fn invalid_input(source: Error, stage: &'static str) -> Error {
    classify(
        source,
        ErrorFacts::before_run(ErrorCategory::InvalidInput, stage),
    )
}

pub(crate) fn parse_remote(stderr: &[u8], already_emitted: bool) -> Option<RemoteError> {
    let text = String::from_utf8_lossy(stderr);
    text.lines().rev().find_map(|line| {
        let envelope: ErrorEnvelope = serde_json::from_str(line).ok()?;
        (envelope.kind == "runlab.error" && envelope.schema_version == 1).then_some(RemoteError {
            envelope,
            already_emitted,
        })
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn is_not_found(error: &Error) -> bool {
    error.chain().any(|item| {
        item.downcast_ref::<RemoteError>()
            .is_some_and(|remote| remote.envelope.category == ErrorCategory::NotFound)
            || item
                .downcast_ref::<ClassifiedError>()
                .is_some_and(|classified| classified.category == ErrorCategory::NotFound)
    })
}

pub(crate) fn emit(error: &Error) {
    if error.chain().any(|item| {
        item.downcast_ref::<RemoteError>()
            .is_some_and(|remote| remote.already_emitted)
    }) {
        return;
    }
    let envelope = envelope(error);
    let mut stderr = std::io::stderr().lock();
    if serde_json::to_writer(&mut stderr, &envelope).is_ok() {
        let _ = writeln!(stderr);
    } else {
        let _ = writeln!(stderr, "{}", envelope.message);
    }
}

fn envelope(error: &Error) -> ErrorEnvelope {
    if let Some(remote) = error
        .chain()
        .find_map(|item| item.downcast_ref::<RemoteError>())
    {
        return remote.envelope.clone();
    }
    if let Some(classified) = error
        .chain()
        .filter_map(|item| item.downcast_ref::<ClassifiedError>())
        .last()
    {
        return ErrorEnvelope {
            schema_version: 1,
            kind: "runlab.error".to_owned(),
            category: classified.category,
            stage: classified.stage.clone(),
            message: format!("{:#}", classified.source),
            run_id: classified.run_id.clone(),
            accepted: classified.accepted,
            run_created: classified.run_created,
            retryable: classified.retryable,
            recovery: classified.recovery.clone(),
        };
    }
    ErrorEnvelope {
        schema_version: 1,
        kind: "runlab.error".to_owned(),
        category: ErrorCategory::Internal,
        stage: "command".to_owned(),
        message: format!("{error:#}"),
        run_id: None,
        accepted: None,
        run_created: None,
        retryable: false,
        recovery: None,
    }
}
