use serde::{Deserialize, Serialize};

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
}

impl Comment {
    pub fn api_value(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "path": self.path, "body": self.body, "line": self.line, "side": "RIGHT"
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
}
