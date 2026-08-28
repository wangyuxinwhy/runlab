//! Version-matched documentation exposed through the CLI.

use std::borrow::Cow;

use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct TopicSummary {
    pub(crate) name: &'static str,
    pub(crate) title: &'static str,
    pub(crate) summary: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct TopicDocument {
    pub(crate) name: &'static str,
    pub(crate) title: &'static str,
    pub(crate) media_type: &'static str,
    pub(crate) content: Cow<'static, str>,
}

struct Topic {
    summary: TopicSummary,
    content: &'static str,
}

const TOPICS: &[Topic] = &[Topic {
    summary: TopicSummary {
        name: "how-to/build-images",
        title: "Build OCI Images for RunLab",
        summary: "Layer, configure, verify, import, and clean up Agent Images.",
    },
    content: include_str!("../docs/how-to/build-images.md"),
}];

#[must_use]
pub(crate) fn list() -> Vec<TopicSummary> {
    TOPICS.iter().map(|topic| topic.summary).collect()
}

pub(crate) fn get(name: &str) -> Result<TopicDocument> {
    let Some(topic) = TOPICS.iter().find(|topic| topic.summary.name == name) else {
        bail!(
            "documentation topic not found: {name:?}\n\
             Hint: run `runlab docs list` to discover valid topic names"
        );
    };
    Ok(TopicDocument {
        name: topic.summary.name,
        title: topic.summary.title,
        media_type: "text/markdown",
        content: normalize_newlines(topic.content),
    })
}

fn normalize_newlines(content: &'static str) -> Cow<'static, str> {
    if content.contains("\r\n") {
        Cow::Owned(content.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(content)
    }
}

#[cfg(test)]
mod tests {
    use super::{get, list, normalize_newlines};

    #[test]
    fn lists_and_gets_bundled_topics() {
        let topics = list();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].name, "how-to/build-images");
        assert!(
            get("how-to/build-images")
                .expect("build Image documentation")
                .content
                .contains("base → agent → repository → task")
        );

        let error = get("missing").expect_err("reject unknown topic");
        let message = error.to_string();
        assert!(message.contains("documentation topic not found"));
        assert!(message.contains("runlab docs list"));
    }

    #[test]
    fn normalizes_bundled_markdown_across_checkout_platforms() {
        assert_eq!(
            normalize_newlines("# Title\r\n\r\nBody\r\n"),
            "# Title\n\nBody\n"
        );
    }
}
