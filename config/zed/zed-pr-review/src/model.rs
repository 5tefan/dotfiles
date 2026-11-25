use serde::{Deserialize, Serialize};

use crate::message::format_agent_body;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PullRequest {
    pub host: String,
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub title: String,
    pub head_sha: String,
    pub base_sha: String,
    pub head_repo: String,
    pub head_ref: String,
    pub author: String,
    pub reviewer: String,
}

impl PullRequest {
    pub fn same_revision(&self, other: &Self) -> bool {
        self.host == other.host
            && self.repo == other.repo
            && self.number == other.number
            && self.head_sha == other.head_sha
            && self.base_sha == other.base_sha
            && self.head_repo == other.head_repo
            && self.head_ref == other.head_ref
            && self.reviewer == other.reviewer
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Comment {
    pub path: String,
    pub start_line: u32,
    pub line: u32,
    pub column: u32,
    pub selected_text: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub agent: bool,
}

impl Comment {
    pub fn api_value(&self) -> serde_json::Value {
        let body = if self.agent {
            format_agent_body(&self.body).unwrap_or_else(|_| self.body.clone())
        } else {
            self.body.clone()
        };
        let mut value = serde_json::json!({
            "path": self.path, "body": body, "line": self.line, "side": "RIGHT"
        });
        if self.start_line != self.line {
            value["start_line"] = self.start_line.into();
            value["start_side"] = "RIGHT".into();
        }
        value
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
    pub previous_filename: Option<String>,
    pub patch: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ThreadComment {
    pub id: String,
    pub database_id: Option<u64>,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub is_collapsed: bool,
    pub path: String,
    pub line: Option<u32>,
    pub original_line: Option<u32>,
    pub start_line: Option<u32>,
    pub diff_side: String,
    pub comments: Vec<ThreadComment>,
    pub pr_number: u64,
    pub bookmark: String,
    pub head_sha: String,
}

impl ReviewThread {
    pub fn effective_line(&self) -> Option<u32> {
        self.line.or(self.original_line)
    }

    pub fn comment_ids(&self) -> Vec<String> {
        self.comments.iter().map(|c| c.id.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_omits_local_metadata_and_single_line_start() {
        let mut comment = Comment {
            path: "a.rs".into(),
            start_line: 3,
            line: 3,
            column: 2,
            selected_text: "text".into(),
            body: "feedback".into(),
            agent: false,
        };
        assert_eq!(
            comment.api_value(),
            serde_json::json!({
                "path": "a.rs", "body": "feedback", "line": 3, "side": "RIGHT"
            })
        );
        comment.line = 5;
        assert_eq!(comment.api_value()["start_line"], 3);
        assert_eq!(comment.api_value()["start_side"], "RIGHT");
    }

    #[test]
    fn api_value_renders_agent_comment_as_blockquote_without_mutating_local_body() {
        let comment = Comment {
            path: "a.rs".into(),
            start_line: 3,
            line: 3,
            column: 1,
            selected_text: "text".into(),
            body: "First paragraph.\n\nSecond paragraph.".into(),
            agent: true,
        };
        assert_eq!(
            comment.api_value()["body"],
            "> First paragraph.\n>\n> Second paragraph.\n\n\\- an agent"
        );
        assert_eq!(comment.body, "First paragraph.\n\nSecond paragraph.");
    }
}
