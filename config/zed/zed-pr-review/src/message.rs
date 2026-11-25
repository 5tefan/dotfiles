use crate::model::Comment;
use anyhow::{Context, Result, ensure};

const REFERENCE_PREFIX: &str = "#|";
const CONTEXT_LINES: usize = 5;
const AGENT_SIGNATURE: &str = r"\- an agent";

/// Deterministically wrap raw Markdown in a continuous blockquote followed by `\- an agent`.
/// Idempotent when given text that is already formatted in this exact shape.
pub fn format_agent_body(raw: &str) -> Result<String> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let first = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .context("Agent comment body must not be blank")?;
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(first);
    let trimmed = &lines[first..=last];

    // Keep idempotent so re-formatting already-formatted agent text does not double-quote or double-sign.
    if trimmed.len() >= 3
        && trimmed[trimmed.len() - 1] == AGENT_SIGNATURE
        && trimmed[trimmed.len() - 2].trim().is_empty()
        && trimmed[..trimmed.len() - 2]
            .iter()
            .all(|line| *line == ">" || line.starts_with("> "))
    {
        return Ok(trimmed.join("\n"));
    }

    let mut out = String::new();
    for (idx, line) in trimmed.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.trim().is_empty() {
            out.push('>');
        } else {
            out.push_str("> ");
            out.push_str(line);
        }
    }
    out.push_str("\n\n");
    out.push_str(AGENT_SIGNATURE);
    Ok(out)
}

pub fn review_template(
    comments: &[Comment],
    mut read_blob: impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<String> {
    if comments.is_empty() {
        return Ok(String::new());
    }
    let mut template = String::from("\n\n");
    reference(
        &mut template,
        "Write your review summary above. Leaving it blank aborts.",
    );
    reference(
        &mut template,
        "Lines starting with #| are removed before submission.",
    );
    reference(
        &mut template,
        "Reference only: > marks commented lines, with up to five context lines on each side.",
    );
    reference(
        &mut template,
        "Editing this section does not change saved comments. Use edit INDEX to change a comment.",
    );

    for (index, comment) in comments.iter().enumerate() {
        let blob = read_blob(&comment.path)
            .with_context(|| format!("Cannot load context for comment {}", index + 1))?;
        let content = std::str::from_utf8(&blob).context("Comment context must be UTF-8 text")?;
        let content = content.replace("\r\n", "\n");
        ensure!(
            !content.contains(['\0', '\r']),
            "Comment context contains NUL or bare CR characters"
        );
        let lines: Vec<_> = content.lines().collect();
        let start = comment.start_line as usize;
        let end = comment.line as usize;
        ensure!(
            start > 0 && start <= end && end <= lines.len(),
            "Comment {} is outside the pinned file",
            index + 1
        );
        let first = start.saturating_sub(CONTEXT_LINES).max(1);
        let last = end.saturating_add(CONTEXT_LINES).min(lines.len());
        let width = last.to_string().len();
        reference(&mut template, "");
        reference(
            &mut template,
            &format!(
                "Comment {}: {}:{}–{} RIGHT",
                index + 1,
                comment.path,
                start,
                end
            ),
        );
        reference(&mut template, "");
        for row in first..=last {
            let marker = if (start..=end).contains(&row) {
                '>'
            } else {
                ' '
            };
            reference(
                &mut template,
                &format!("{marker} {row:>width$} | {}", lines[row - 1]),
            );
        }
        reference(&mut template, "");
        reference(
            &mut template,
            if comment.agent {
                "Your comment [agent]:"
            } else {
                "Your comment:"
            },
        );
        reference(&mut template, &comment.body);
    }
    Ok(template)
}

// Prefix every physical line, including lines embedded in paths or comment bodies.
// Normalize CR here too so an editor cannot turn it into an unprefixed line.
fn reference(template: &mut String, text: &str) {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in text.split('\n') {
        template.push_str(REFERENCE_PREFIX);
        if !line.is_empty() {
            template.push(' ');
            template.push_str(line);
        }
        template.push('\n');
    }
}

/// Reserve #| for references rather than stripping Markdown headings from summaries.
pub fn review_summary(message: &str) -> String {
    message
        .split_inclusive('\n')
        .filter(|line| !line.trim_start().starts_with(REFERENCE_PREFIX))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(start_line: u32, line: u32) -> Comment {
        Comment {
            path: "src/example.rs".into(),
            start_line,
            line,
            column: 1,
            selected_text: "selected text".into(),
            body: "Please check this.\n\nMore detail.".into(),
            agent: false,
        }
    }

    #[test]
    fn includes_entire_selection_and_exactly_five_lines_on_each_side() {
        let content = (1..=20)
            .map(|line| format!("source {line}\n"))
            .collect::<String>();
        let template = review_template(&[comment(7, 9)], |path| {
            assert_eq!(path, "src/example.rs");
            Ok(content.as_bytes().to_vec())
        })
        .unwrap();
        assert!(template.starts_with("\n\n#|"));
        assert!(template.contains("#| Comment 1: src/example.rs:7–9 RIGHT\n"));
        for row in 2..=14 {
            let marker = if (7..=9).contains(&row) { '>' } else { ' ' };
            assert!(template.contains(&format!("#| {marker} {row:>2} | source {row}\n")));
        }
        assert!(!template.contains("| source 1\n"));
        assert!(!template.contains("| source 15\n"));
        assert!(
            template.ends_with("#| Your comment:\n#| Please check this.\n#|\n#| More detail.\n")
        );
        assert!(review_summary(&template).trim().is_empty());
    }

    #[test]
    fn clips_context_to_file_boundaries_and_handles_crlf_and_no_final_newline() {
        for content in [
            "one\ntwo\nthree",
            "one\ntwo\nthree\n",
            "one\r\ntwo\r\nthree\r\n",
        ] {
            let template = review_template(&[comment(1, 1), comment(3, 3)], |_| {
                Ok(content.as_bytes().to_vec())
            })
            .unwrap();
            assert!(template.contains("#| > 1 | one\n#|   2 | two\n#|   3 | three\n"));
            assert!(template.contains("#|   1 | one\n#|   2 | two\n#| > 3 | three\n"));
            assert!(!template.contains("4 |"));
            assert!(!template.contains('\r'));
            assert!(review_summary(&template).trim().is_empty());
        }
    }

    #[test]
    fn every_line_of_unusual_comment_bodies_is_reference_only() {
        let mut comment = comment(1, 1);
        comment.body = "# Heading\r\n```rust\n#| text\n```\rBare CR\n\nLast line\n".into();
        let template = review_template(&[comment], |_| Ok(b"one\n".to_vec())).unwrap();
        assert!(template.contains("#| ```\n#| Bare CR\n"));
        assert!(review_summary(&template).trim().is_empty());
    }

    #[test]
    fn cleanup_preserves_summary_markdown_and_only_removes_reference_lines() {
        let message = "# Summary\r\n\r\nLooks good.\r\n#| context\r\n  #| edited reference\r\n## Follow-up\r\nA literal #| inside prose stays.\r\n#|";
        assert_eq!(
            review_summary(message),
            "# Summary\r\n\r\nLooks good.\r\n## Follow-up\r\nA literal #| inside prose stays.\r\n"
        );
        assert_eq!(
            review_summary("Summary without newline"),
            "Summary without newline"
        );
        assert!(review_summary("\n\t#| reference only\n").trim().is_empty());
    }

    #[test]
    fn no_comments_keeps_the_editor_empty_without_loading_blobs() {
        assert_eq!(
            review_template(&[], |_| panic!("No blob needed")).unwrap(),
            ""
        );
    }

    #[test]
    fn invalid_context_aborts_instead_of_showing_wrong_lines() {
        for (start, end) in [(0, 1), (2, 1), (1, 4)] {
            assert!(
                review_template(
                    &[comment(start, end)],
                    |_| Ok(b"one\ntwo\nthree\n".to_vec())
                )
                .is_err()
            );
        }
        for content in [b"one\0".as_slice(), b"one\rtwo", &[0xff]] {
            assert!(review_template(&[comment(1, 1)], |_| Ok(content.to_vec())).is_err());
        }
        assert!(review_template(&[comment(1, 1)], |_| anyhow::bail!("Missing blob")).is_err());
    }

    #[test]
    fn format_agent_body_wraps_paragraphs_code_fences_lists_and_crlf() {
        let raw = "\n\nbla bla bla\r\n\r\n```rust\r\ncode...\r\n```\r\n\r\n- item 1\r\n- item 2\r\n\r\ntherefore...\n\n";
        let expected = concat!(
            "> bla bla bla\n",
            ">\n",
            "> ```rust\n",
            "> code...\n",
            "> ```\n",
            ">\n",
            "> - item 1\n",
            "> - item 2\n",
            ">\n",
            "> therefore...\n",
            "\n",
            r"\- an agent",
        );
        let formatted = format_agent_body(raw).unwrap();
        assert_eq!(formatted, expected);
        // Idempotent: formatting already-formatted agent text does not double-quote or double-sign.
        assert_eq!(format_agent_body(&formatted).unwrap(), expected);
    }

    #[test]
    fn format_agent_body_rejects_blank_input_and_preserves_indentation() {
        assert!(format_agent_body("").is_err());
        assert!(format_agent_body("   \r\n\t\n  ").is_err());
        assert_eq!(
            format_agent_body("    indented line\n  next").unwrap(),
            ">     indented line\n>   next\n\n\\- an agent"
        );
    }
}
