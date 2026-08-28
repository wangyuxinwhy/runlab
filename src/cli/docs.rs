use std::io::Write as _;

use anyhow::{Context as _, Result};
use clap::{Subcommand, ValueEnum};
use serde::Serialize;

use crate::docs;

use super::emit;

const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Subcommand)]
pub(super) enum DocsCommand {
    /// List bundled documentation topics.
    List,
    /// Get one bundled documentation topic.
    Get {
        /// Documentation topic name returned by `runlab docs list`.
        topic: String,
        /// Write Markdown directly or return a compact JSON document.
        #[arg(long, value_enum, default_value = "markdown")]
        output: DocsOutput,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(super) enum DocsOutput {
    Markdown,
    Json,
}

#[derive(Serialize)]
struct DocsList {
    schema_version: u8,
    topics: Vec<docs::TopicSummary>,
}

#[derive(Serialize)]
struct DocsGet {
    schema_version: u8,
    topic: docs::TopicDocument,
}

pub(super) fn execute(command: DocsCommand) -> Result<u8> {
    match command {
        DocsCommand::List => emit(&DocsList {
            schema_version: SCHEMA_VERSION,
            topics: docs::list(),
        })?,
        DocsCommand::Get { topic, output } => {
            let topic = docs::get(&topic)?;
            match output {
                DocsOutput::Markdown => emit_markdown(&topic.content)?,
                DocsOutput::Json => emit(&DocsGet {
                    schema_version: SCHEMA_VERSION,
                    topic,
                })?,
            }
        }
    }
    Ok(0)
}

fn emit_markdown(content: &str) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout
        .write_all(content.as_bytes())
        .context("failed to write Markdown output")?;
    if !content.ends_with('\n') {
        writeln!(stdout).context("failed to terminate Markdown output")?;
    }
    Ok(())
}
