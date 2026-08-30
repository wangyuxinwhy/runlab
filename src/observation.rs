use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::{Uuid, Version};

use crate::storage::{
    Database, NewObservation, NewObservationRetraction, NewObservationType, ObservationInsertion,
    ObservationRetractionInsertion, ObservationTypeInsertion, StoredObservation,
    StoredObservationRetraction, StoredObservationType,
};

const DOCUMENT_SCHEMA_VERSION: u32 = 1;
pub(crate) const TOKEN_USAGE_TYPE: &str = "runlab/token_usage@v1";
const MAX_TYPE_BYTES: usize = 256;
const MAX_TYPE_TITLE_BYTES: usize = 256;
const MAX_TYPE_DESCRIPTION_BYTES: usize = 64 * 1024;
const MAX_METHOD_TEXT_BYTES: usize = 128;
const MAX_REASON_BYTES: usize = 4096;
const JSON_SCHEMA_DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservationId(Uuid);

impl FromStr for ObservationId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let uuid = Uuid::parse_str(value).context("Observation identity must be a UUID v4")?;
        if uuid.get_version() != Some(Version::Random) || value != uuid.hyphenated().to_string() {
            bail!("Observation identity must use the canonical lowercase UUID v4 form");
        }
        Ok(Self(uuid))
    }
}

impl fmt::Display for ObservationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl Serialize for ObservationId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ObservationId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservationTypeDocument {
    schema_version: u32,
    #[serde(rename = "type")]
    observation_type: String,
    title: String,
    description: String,
    payload_schema: Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservationDocument {
    schema_version: u32,
    observation_id: ObservationId,
    run_id: crate::run::RunId,
    #[serde(rename = "type")]
    observation_type: String,
    method: ObservationMethod,
    payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supersedes_observation_id: Option<ObservationId>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ObservationMethod {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservationRetractionDocument {
    schema_version: u32,
    retraction_id: ObservationId,
    observation_id: ObservationId,
    reason: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ObservationTypeRegistrationResult {
    schema_version: u32,
    kind: &'static str,
    created: bool,
    observation_type: ObservationTypeReport,
}

#[derive(Debug, Serialize)]
pub(crate) struct ObservationTypeGetResult {
    schema_version: u32,
    kind: &'static str,
    observation_type: ObservationTypeReport,
}

#[derive(Debug, Serialize)]
pub(crate) struct ObservationTypeListResult {
    schema_version: u32,
    kind: &'static str,
    observation_types: Vec<ObservationTypeReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_after: Option<String>,
}

#[derive(Debug, Serialize)]
struct ObservationTypeReport {
    schema_version: u32,
    #[serde(rename = "type")]
    observation_type: String,
    title: String,
    description: String,
    payload_schema: Value,
    registered_at: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ObservationSubmitResult {
    schema_version: u32,
    kind: &'static str,
    created: bool,
    observation: ObservationReport,
}

#[derive(Debug, Serialize)]
struct ObservationReport {
    observation_id: String,
    run_id: String,
    #[serde(rename = "type")]
    observation_type: String,
    submitted_at: String,
    method: Value,
    payload: Value,
    supersedes_observation_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ObservationRetractionResult {
    schema_version: u32,
    kind: &'static str,
    created: bool,
    retraction: ObservationRetractionReport,
}

#[derive(Debug, Serialize)]
struct ObservationRetractionReport {
    retraction_id: String,
    observation_id: String,
    retracted_at: String,
    reason: String,
}

pub(crate) fn parse_type_definition(bytes: &[u8]) -> Result<ObservationTypeDocument> {
    let document: ObservationTypeDocument =
        serde_json::from_slice(bytes).context("Observation Type document is not valid JSON")?;
    document.validate()?;
    Ok(document)
}

pub(crate) fn parse_submission(bytes: &[u8]) -> Result<ObservationDocument> {
    let document: ObservationDocument =
        serde_json::from_slice(bytes).context("Observation document is not valid JSON")?;
    document.validate()?;
    Ok(document)
}

pub(crate) fn parse_retraction(bytes: &[u8]) -> Result<ObservationRetractionDocument> {
    let document: ObservationRetractionDocument =
        serde_json::from_slice(bytes).context("Observation retraction is not valid JSON")?;
    document.validate()?;
    Ok(document)
}

impl ObservationTypeDocument {
    fn validate(&self) -> Result<()> {
        if self.schema_version != DOCUMENT_SCHEMA_VERSION {
            bail!("Observation Type schema_version is unsupported");
        }
        validate_type_name(&self.observation_type)?;
        validate_bounded_text(&self.title, MAX_TYPE_TITLE_BYTES, "Observation Type title")?;
        validate_description(&self.description)?;
        validate_payload_schema(&self.payload_schema)
    }

    fn payload_schema_json(&self) -> Result<String> {
        serde_json::to_string(&self.payload_schema).map_err(Into::into)
    }
}

impl ObservationDocument {
    fn validate(&self) -> Result<()> {
        if self.schema_version != DOCUMENT_SCHEMA_VERSION {
            bail!("Observation schema_version is unsupported");
        }
        validate_type_name(&self.observation_type)?;
        validate_bounded_text(&self.method.name, MAX_METHOD_TEXT_BYTES, "Method name")?;
        validate_bounded_text(
            &self.method.version,
            MAX_METHOD_TEXT_BYTES,
            "Method version",
        )?;
        if self
            .supersedes_observation_id
            .as_ref()
            .is_some_and(|superseded| superseded == &self.observation_id)
        {
            bail!("an Observation cannot supersede itself");
        }
        Ok(())
    }
}

impl ObservationRetractionDocument {
    fn validate(&self) -> Result<()> {
        if self.schema_version != DOCUMENT_SCHEMA_VERSION {
            bail!("Observation retraction schema_version is unsupported");
        }
        validate_bounded_text(&self.reason, MAX_REASON_BYTES, "retraction reason")
    }
}

pub(crate) fn register_type(
    database: &Database,
    document: &ObservationTypeDocument,
) -> Result<ObservationTypeRegistrationResult> {
    if document.observation_type.starts_with("runlab/") {
        return Err(crate::error::invalid_input(
            anyhow::anyhow!("the runlab/ Observation Type namespace is reserved"),
            "observation_type_register",
        ));
    }
    let payload_schema_json = document.payload_schema_json()?;
    let registered_at = Utc::now().to_rfc3339();
    let insertion = database.observation_type_insert(&NewObservationType {
        observation_type: &document.observation_type,
        registered_at: &registered_at,
        title: &document.title,
        description: &document.description,
        payload_schema_json: &payload_schema_json,
    })?;
    let (created, stored) = match insertion {
        ObservationTypeInsertion::Created(stored) => (true, stored),
        ObservationTypeInsertion::Existing(stored) => (false, stored),
        ObservationTypeInsertion::Conflict => {
            return Err(crate::error::classify(
                anyhow::anyhow!(
                    "Observation Type is already registered with a different definition: {}",
                    document.observation_type
                ),
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::Conflict,
                    "observation_type_register",
                ),
            ));
        }
    };
    Ok(ObservationTypeRegistrationResult {
        schema_version: 1,
        kind: "runlab.observation_type_registration",
        created,
        observation_type: type_report(stored)?,
    })
}

pub(crate) fn get_type(
    database: &Database,
    observation_type: &str,
) -> Result<ObservationTypeGetResult> {
    validate_type_name(observation_type)
        .map_err(|error| crate::error::invalid_input(error, "observation_type_get"))?;
    let stored = database
        .observation_type_get(observation_type)?
        .ok_or_else(|| {
            crate::error::classify(
                anyhow::anyhow!("Observation Type is not registered: {observation_type}"),
                crate::error::ErrorFacts::before_run(
                    crate::error::ErrorCategory::NotFound,
                    "observation_type_get",
                ),
            )
        })?;
    Ok(ObservationTypeGetResult {
        schema_version: 1,
        kind: "runlab.observation_type",
        observation_type: type_report(stored)?,
    })
}

pub(crate) fn list_types(
    database: &Database,
    limit: usize,
    after: Option<&str>,
) -> Result<ObservationTypeListResult> {
    if !(1..=1000).contains(&limit) {
        return Err(crate::error::invalid_input(
            anyhow::anyhow!("limit must be between 1 and 1000"),
            "observation_type_list",
        ));
    }
    if let Some(after) = after {
        validate_type_name(after)
            .map_err(|error| crate::error::invalid_input(error, "observation_type_list"))?;
    }
    let mut stored = database.observation_type_list(limit + 1, after)?;
    let next_after = (stored.len() > limit).then(|| stored[limit - 1].observation_type.clone());
    stored.truncate(limit);
    Ok(ObservationTypeListResult {
        schema_version: 1,
        kind: "runlab.observation_type_list",
        observation_types: stored
            .into_iter()
            .map(type_report)
            .collect::<Result<Vec<_>>>()?,
        next_after,
    })
}

pub(crate) fn submit(
    database: &Database,
    document: &ObservationDocument,
) -> Result<ObservationSubmitResult> {
    let registered_type = database
        .observation_type_get(&document.observation_type)?
        .ok_or_else(|| {
            crate::error::invalid_input(
                anyhow::anyhow!(
                    "Observation Type is not registered: {}\nHint: run `runlab observation type list` to discover registered Types",
                    document.observation_type
                ),
                "observation_input",
            )
        })?;
    let payload_schema: Value = serde_json::from_str(&registered_type.payload_schema_json)
        .context("stored Observation Type payload_schema is invalid")?;
    validate_payload(&payload_schema, &document.payload).map_err(|error| {
        crate::error::invalid_input(
            error.context(format!(
                "payload does not satisfy Observation Type {}",
                document.observation_type
            )),
            "observation_input",
        )
    })?;

    let observation_id = document.observation_id.to_string();
    let run_id = document.run_id.to_string();
    let method_json = serde_json::to_string(&document.method)?;
    let payload_json = serde_json::to_string(&document.payload)?;
    let supersedes = document
        .supersedes_observation_id
        .as_ref()
        .map(ToString::to_string);
    let submitted_at = Utc::now().to_rfc3339();
    let insertion = database.observation_insert(&NewObservation {
        observation_id: &observation_id,
        run_id: &run_id,
        observation_type: &document.observation_type,
        submitted_at: &submitted_at,
        method_json: &method_json,
        payload_json: &payload_json,
        supersedes_observation_id: supersedes.as_deref(),
    })?;
    let (created, stored) = match insertion {
        ObservationInsertion::Created(stored) => (true, stored),
        ObservationInsertion::Existing(stored) => (false, stored),
        other => return Err(classify_insertion(&other, &run_id, &observation_id)),
    };
    Ok(ObservationSubmitResult {
        schema_version: 1,
        kind: "runlab.observation_submission",
        created,
        observation: observation_report(stored)?,
    })
}

pub(crate) fn retract(
    database: &Database,
    document: &ObservationRetractionDocument,
) -> Result<ObservationRetractionResult> {
    let retraction_id = document.retraction_id.to_string();
    let observation_id = document.observation_id.to_string();
    let retracted_at = Utc::now().to_rfc3339();
    let insertion = database.observation_retract(&NewObservationRetraction {
        retraction_id: &retraction_id,
        observation_id: &observation_id,
        retracted_at: &retracted_at,
        reason: &document.reason,
    })?;
    let (created, stored) = match insertion {
        ObservationRetractionInsertion::Created(stored) => (true, stored),
        ObservationRetractionInsertion::Existing(stored) => (false, stored),
        other => return Err(classify_retraction(&other, &observation_id, &retraction_id)),
    };
    Ok(ObservationRetractionResult {
        schema_version: 1,
        kind: "runlab.observation_retraction_submission",
        created,
        retraction: retraction_report(stored),
    })
}

pub(crate) fn builtin_token_usage_type() -> ObservationTypeDocument {
    ObservationTypeDocument {
        schema_version: 1,
        observation_type: TOKEN_USAGE_TYPE.to_owned(),
        title: "Agent token usage".to_owned(),
        description: "Cumulative token usage attributed to one Run by the declared Method. input_tokens includes ordinary input, cache reads, and cache writes; cached_input_tokens and cache_write_input_tokens are optional reported subsets and must not exceed input_tokens when known. output_tokens includes reasoning output; reasoning_output_tokens is an optional reported subset and must not exceed output_tokens when known. null means the Method cannot report that subset, not zero. coverage=complete means the Method established complete cumulative input and output coverage for the Run; coverage=partial means the reported values are a reliable known lower bound. When cumulative input or output usage is unavailable, the Method must not submit this Type. Total tokens are derived as input_tokens + output_tokens and are not stored in the payload.".to_owned(),
        payload_schema: serde_json::json!({
            "$schema": JSON_SCHEMA_DRAFT_2020_12,
            "type": "object",
            "additionalProperties": false,
            "required": [
                "coverage", "input_tokens", "cached_input_tokens",
                "cache_write_input_tokens", "output_tokens", "reasoning_output_tokens"
            ],
            "properties": {
                "coverage": {"enum": ["complete", "partial"]},
                "input_tokens": {"type": "integer", "minimum": 0},
                "cached_input_tokens": {"type": ["integer", "null"], "minimum": 0},
                "cache_write_input_tokens": {"type": ["integer", "null"], "minimum": 0},
                "output_tokens": {"type": "integer", "minimum": 0},
                "reasoning_output_tokens": {"type": ["integer", "null"], "minimum": 0}
            }
        }),
    }
}

pub(crate) fn builtin_token_usage_parts() -> Result<(String, String, String)> {
    let document = builtin_token_usage_type();
    let payload_schema_json = document.payload_schema_json()?;
    Ok((document.title, document.description, payload_schema_json))
}

fn type_report(stored: StoredObservationType) -> Result<ObservationTypeReport> {
    Ok(ObservationTypeReport {
        schema_version: 1,
        observation_type: stored.observation_type,
        title: stored.title,
        description: stored.description,
        payload_schema: serde_json::from_str(&stored.payload_schema_json)
            .context("stored Observation Type payload_schema is invalid")?,
        registered_at: stored.registered_at,
    })
}

fn observation_report(stored: StoredObservation) -> Result<ObservationReport> {
    Ok(ObservationReport {
        observation_id: stored.observation_id,
        run_id: stored.run_id,
        observation_type: stored.observation_type,
        submitted_at: stored.submitted_at,
        method: serde_json::from_str(&stored.method_json)
            .context("stored Observation Method is invalid")?,
        payload: serde_json::from_str(&stored.payload_json)
            .context("stored Observation payload is invalid")?,
        supersedes_observation_id: stored.supersedes_observation_id,
    })
}

fn retraction_report(stored: StoredObservationRetraction) -> ObservationRetractionReport {
    ObservationRetractionReport {
        retraction_id: stored.retraction_id,
        observation_id: stored.observation_id,
        retracted_at: stored.retracted_at,
        reason: stored.reason,
    }
}

fn classify_insertion(
    insertion: &ObservationInsertion,
    run_id: &str,
    observation_id: &str,
) -> anyhow::Error {
    let (category, message, recovery) = match insertion {
        ObservationInsertion::IdentityConflict => (
            crate::error::ErrorCategory::Conflict,
            format!("Observation identity is already bound to different content: {observation_id}"),
            None,
        ),
        ObservationInsertion::RunNotFound => (
            crate::error::ErrorCategory::NotFound,
            format!("Run does not exist: {run_id}"),
            None,
        ),
        ObservationInsertion::RunNotTerminal => (
            crate::error::ErrorCategory::Conflict,
            format!("Observation requires a terminal Run: {run_id}"),
            Some(format!("runlab run get {run_id}")),
        ),
        ObservationInsertion::SupersededNotFound => (
            crate::error::ErrorCategory::NotFound,
            "superseded Observation does not exist".to_owned(),
            None,
        ),
        ObservationInsertion::SupersededMismatch => (
            crate::error::ErrorCategory::Conflict,
            "an Observation can only supersede an active Observation with the same Run and Type"
                .to_owned(),
            None,
        ),
        ObservationInsertion::Created(_) | ObservationInsertion::Existing(_) => {
            unreachable!("successful insertion was handled by the caller")
        }
    };
    crate::error::classify(
        anyhow::anyhow!(message),
        observation_error_facts(category, run_id, true, recovery),
    )
}

fn classify_retraction(
    insertion: &ObservationRetractionInsertion,
    observation_id: &str,
    retraction_id: &str,
) -> anyhow::Error {
    let (category, message) = match insertion {
        ObservationRetractionInsertion::IdentityConflict => (
            crate::error::ErrorCategory::Conflict,
            format!("retraction identity is already bound to different content: {retraction_id}"),
        ),
        ObservationRetractionInsertion::ObservationNotFound => (
            crate::error::ErrorCategory::NotFound,
            format!("Observation does not exist: {observation_id}"),
        ),
        ObservationRetractionInsertion::ObservationInactive => (
            crate::error::ErrorCategory::Conflict,
            format!("only an active Observation can be retracted: {observation_id}"),
        ),
        ObservationRetractionInsertion::Created(_)
        | ObservationRetractionInsertion::Existing(_) => {
            unreachable!("successful retraction was handled by the caller")
        }
    };
    crate::error::classify(
        anyhow::anyhow!(message),
        crate::error::ErrorFacts::before_run(category, "observation_retract"),
    )
}

fn observation_error_facts(
    category: crate::error::ErrorCategory,
    run_id: &str,
    accepted: bool,
    recovery: Option<String>,
) -> crate::error::ErrorFacts {
    crate::error::ErrorFacts {
        category,
        stage: "observation_submit",
        run_id: Some(run_id.to_owned()),
        accepted: Some(accepted),
        run_created: Some(false),
        retryable: false,
        recovery,
    }
}

fn validate_payload_schema(schema: &Value) -> Result<()> {
    if !schema.is_object() {
        bail!("payload_schema must be a JSON Schema object");
    }
    if schema.get("$schema").and_then(Value::as_str) != Some(JSON_SCHEMA_DRAFT_2020_12) {
        bail!("payload_schema must declare JSON Schema Draft 2020-12 in $schema");
    }
    jsonschema::draft202012::meta::validate(schema)
        .map_err(|error| anyhow::anyhow!("payload_schema is invalid: {error}"))?;
    jsonschema::draft202012::options()
        .offline()
        .build(schema)
        .map_err(|error| anyhow::anyhow!("payload_schema cannot be compiled offline: {error}"))?;
    Ok(())
}

fn validate_payload(schema: &Value, payload: &Value) -> Result<()> {
    let validator = jsonschema::draft202012::options()
        .offline()
        .build(schema)
        .context("registered payload_schema cannot be compiled offline")?;
    if let Some(error) = validator.iter_errors(payload).next() {
        bail!("{error}");
    }
    Ok(())
}

fn validate_type_name(value: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value || value.len() > MAX_TYPE_BYTES {
        bail!("Observation Type must be non-empty, canonical, and at most {MAX_TYPE_BYTES} bytes");
    }
    let Some((unversioned, version)) = value.rsplit_once("@v") else {
        bail!("Observation Type must end with @v followed by a positive decimal version");
    };
    if version.is_empty()
        || version.starts_with('0')
        || !version.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("Observation Type version must be a positive canonical decimal integer");
    }
    let Some((namespace, name)) = unversioned.split_once('/') else {
        bail!("Observation Type must use namespace/name@vN");
    };
    if namespace.is_empty() || name.is_empty() || name.contains('/') {
        bail!("Observation Type must use namespace/name@vN");
    }
    for component in [namespace, name] {
        if !component.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        }) || !component
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            || !component
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            bail!(
                "Observation Type namespace and name must use lowercase ASCII letters, digits, '.', '_', or '-', and start and end with a letter or digit"
            );
        }
    }
    Ok(())
}

fn validate_description(value: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value {
        bail!("Observation Type description must be non-empty and have no surrounding whitespace");
    }
    if value.len() > MAX_TYPE_DESCRIPTION_BYTES {
        bail!("Observation Type description exceeds {MAX_TYPE_DESCRIPTION_BYTES} UTF-8 bytes");
    }
    if value.contains('\0') {
        bail!("Observation Type description contains NUL");
    }
    Ok(())
}

fn validate_bounded_text(value: &str, limit: usize, label: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value {
        bail!("{label} must be non-empty and have no surrounding whitespace");
    }
    if value.len() > limit {
        bail!("{label} exceeds {limit} UTF-8 bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("{label} contains a control character");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_token_usage_is_an_ordinary_valid_type() {
        builtin_token_usage_type()
            .validate()
            .expect("built-in Type");
    }

    #[test]
    fn observation_documents_are_type_agnostic() {
        let document = parse_submission(
            br#"{
              "schema_version":1,
              "observation_id":"550e8400-e29b-41d4-a716-446655440010",
              "run_id":"550e8400-e29b-41d4-a716-446655440000",
              "type":"example/score@v1",
              "method":{"name":"example/scorer","version":"1.0.0"},
              "payload":{"score":0.8}
            }"#,
        )
        .expect("Observation document");
        assert_eq!(document.observation_type, "example/score@v1");
    }

    #[test]
    fn token_usage_schema_rejects_total_tokens_and_missing_nullables() {
        let schema = builtin_token_usage_type().payload_schema;
        let payload = serde_json::json!({
            "coverage": "complete", "input_tokens": 12, "cached_input_tokens": 8,
            "cache_write_input_tokens": null, "output_tokens": 3,
            "reasoning_output_tokens": null, "total_tokens": 15
        });
        assert!(validate_payload(&schema, &payload).is_err());
        let payload = serde_json::json!({
            "coverage": "complete", "input_tokens": 12, "cached_input_tokens": 8,
            "cache_write_input_tokens": null, "output_tokens": 3
        });
        assert!(validate_payload(&schema, &payload).is_err());
    }
}
