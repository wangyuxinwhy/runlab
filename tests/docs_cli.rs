use std::process::{Command, Output};

use serde_json::{Value, json};

const TOPIC: &str = "how-to/build-images";

#[test]
fn docs_are_discoverable_without_state_or_managed_vm() {
    let top = run(&["--help"]);
    assert_success(&top);
    let top = text(&top.stdout);
    assert!(top.contains("\n  docs "));
    assert!(top.contains("runlab docs get start-here"));

    let help = run(&["docs", "--help"]);
    assert_success(&help);
    let help = text(&help.stdout);
    assert!(help.contains("list"));
    assert!(help.contains("get"));
    assert!(!help.contains("search"));

    let listed = run_without_runtime_state(&["docs", "list"]);
    assert_success(&listed);
    assert_eq!(
        json_output(&listed),
        json!({
            "schema_version": 1,
            "topics": [
                {
                    "name": "start-here",
                    "title": "Start Here",
                    "summary": "Run one complete Image-to-Final-Environment workflow."
                },
                {
                    "name": TOPIC,
                    "title": "Build OCI Images for RunLab",
                    "summary": "Layer, configure, verify, import, and clean up Agent Images."
                },
                {
                    "name": "how-to/query-runs",
                    "title": "Query Runs",
                    "summary": "Discover and query bounded Run selection facts with read-only SQL."
                },
                {
                    "name": "how-to/delete-runs",
                    "title": "Delete Terminal Runs",
                    "summary": "Select, preview, and permanently delete bounded terminal Run assets."
                }
            ]
        })
    );
}

#[test]
fn docs_get_returns_markdown_or_compact_json() {
    let start = run(&["docs", "get", "start-here"]);
    assert_success(&start);
    let start = text(&start.stdout);
    assert!(start.starts_with("# Start Here\n"));
    assert!(start.contains("runlab filesystem get"));
    assert!(start.contains("runlab.error"));

    let markdown = run(&["docs", "get", TOPIC]);
    assert_success(&markdown);
    let markdown = text(&markdown.stdout);
    assert!(markdown.starts_with("# Build OCI Images for RunLab\n"));
    assert!(markdown.contains("base → agent → repository → task"));
    assert!(markdown.contains("RunLab consumes standard OCI Images; it does not build them."));
    assert!(markdown.ends_with('\n'));

    let json = run(&["docs", "get", TOPIC, "--output", "json"]);
    assert_success(&json);
    let json = json_output(&json);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["topic"]["name"], TOPIC);
    assert_eq!(json["topic"]["title"], "Build OCI Images for RunLab");
    assert_eq!(json["topic"]["media_type"], "text/markdown");
    assert_eq!(json["topic"]["content"], markdown);

    let query = run(&["docs", "get", "how-to/query-runs"]);
    assert_success(&query);
    let query = text(&query.stdout);
    assert!(query.starts_with("# Query Runs\n"));
    assert!(query.contains("runlab schema get runs"));
}

#[test]
fn docs_get_rejects_unknown_topic_without_success_output() {
    let output = run(&["docs", "get", "missing"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = serde_json::from_slice::<Value>(&output.stderr).expect("structured error");
    assert_eq!(error["kind"], "runlab.error");
    assert!(error["message"].as_str().is_some_and(|message| {
        message.contains("documentation topic not found: \"missing\"")
            && message.contains("run `runlab docs list`")
    }));
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runlab"))
        .args(arguments)
        .output()
        .expect("runlab process")
}

fn run_without_runtime_state(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runlab"))
        .env("RUNLAB_STATE", "/path/that/must/not/be/read")
        .env("RUNLAB_LIMACTL", "/path/that/must/not/be/executed")
        .args(arguments)
        .output()
        .expect("runlab process")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={} stderr={}",
        output.status,
        text(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        text(&output.stderr)
    );
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("invalid JSON: {error}; stdout={}", text(&output.stdout)))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
