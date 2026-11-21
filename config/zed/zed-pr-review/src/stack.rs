//! Read-only stacks defined solely by exact repository/branch relationships.
//!
//! A discovery checks a paginated listing against individual PR reads. GitHub does
//! not offer an atomic snapshot; callers must revalidate after a user chooses a PR.

use crate::github::Client;
use crate::model::PullRequest;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

#[derive(Clone, Debug)]
pub struct StackEntry {
    pub pr: PullRequest,
    pub base_ref: String,
    pub draft: bool,
    pub state: String,
    // Keep the API's exact spelling for branch identity, independently of the
    // client's repository spelling in the session pins.
    base_repo: String,
}

impl StackEntry {
    fn matches_pr(&self, pr: &PullRequest) -> bool {
        self.pr.same_revision(pr) && self.pr.url == pr.url && self.pr.author == pr.author
    }

    fn same_pins(&self, other: &Self) -> bool {
        self.matches_pr(&other.pr)
            && self.base_repo == other.base_repo
            && self.base_ref == other.base_ref
            && self.state == other.state
    }
}

/// The connected component containing the pinned current PR, not all PRs that
/// happen to share its base branch. Construction rejects ambiguous parents.
#[derive(Clone, Debug)]
pub struct Stack {
    current: u64,
    root: u64,
    entries: BTreeMap<u64, StackEntry>,
    parents: BTreeMap<u64, u64>,
    children: BTreeMap<u64, Vec<u64>>,
}

impl Stack {
    pub fn discover(client: &Client, current: &PullRequest) -> Result<Self> {
        ensure!(
            client.host == current.host
                && client.repo == current.repo
                && client.reviewer == current.reviewer
                && current.number > 0,
            "Current PR host, repository, number, or reviewer does not match this client"
        );
        let reviewer = client.authenticated_reviewer()?;
        ensure!(reviewer == current.reviewer, "The pinned reviewer changed");
        let before = read_entry(client, current.number, &reviewer)?;
        ensure!(
            before.matches_pr(current),
            "Current PR head, base, or identity changed since the session was pinned"
        );
        let values: Vec<Value> = client.get_pages(&format!(
            "repos/{}/pulls?state=open&per_page=100",
            client.repo
        ))?;
        let stack = Self::from_listing(client, current.number, &reviewer, values)?;
        ensure!(
            before.same_pins(stack.current()),
            "Current PR changed during stack discovery; retry"
        );
        // The list can span multiple pages and moments in time. Check every node
        // that we will display, including base refs absent from PullRequest pins.
        for entry in stack.entries.values() {
            let fresh = read_entry(client, entry.pr.number, &reviewer)?;
            ensure!(
                entry.same_pins(&fresh),
                "PR #{} changed during stack discovery; retry",
                entry.pr.number
            );
        }
        ensure!(
            client.authenticated_reviewer()? == reviewer,
            "The pinned reviewer changed during stack discovery"
        );
        Ok(stack)
    }

    pub fn current(&self) -> &StackEntry {
        &self.entries[&self.current]
    }

    pub fn parent(&self) -> Result<Option<&StackEntry>> {
        Ok(self.parents.get(&self.current).map(|n| &self.entries[n]))
    }

    /// Explicit candidates in PR-number order. Multiple children are never
    /// reduced to one candidate, even if their branches or titles look similar.
    pub fn children(&self) -> Vec<&StackEntry> {
        self.child_numbers(self.current)
            .iter()
            .map(|n| &self.entries[n])
            .collect()
    }

    /// Verify a selected parent/child after `client.read_pr(candidate.pr.number)`.
    /// Rediscovery also checks base refs and rejects a new ambiguous parent or
    /// changes to the child candidates when navigating to a child.
    /// Run this immediately before opening the refreshed PR, not before prompting.
    pub fn verify_adjacent(&self, client: &Client, refreshed: &PullRequest) -> Result<()> {
        let number = refreshed.number;
        let was_parent = self.parents.get(&self.current) == Some(&number);
        ensure!(
            was_parent || self.child_numbers(self.current).contains(&number),
            "PR #{number} is not an adjacent stack candidate"
        );
        let selected = &self.entries[&number];
        ensure!(
            selected.matches_pr(refreshed),
            "Selected PR #{number} changed since stack discovery; retry"
        );
        let fresh = Self::discover(client, &self.current().pr)?;
        ensure!(
            self.current().same_pins(fresh.current()),
            "Current PR base branch or identity changed since stack discovery; retry"
        );
        let still_adjacent = if was_parent {
            fresh.parents.get(&fresh.current) == Some(&number)
        } else {
            ensure!(
                self.child_numbers(self.current) == fresh.child_numbers(fresh.current),
                "Stack child candidates changed; retry to reopen the choices"
            );
            fresh.child_numbers(fresh.current).contains(&number)
        };
        ensure!(still_adjacent, "Selected stack relationship changed; retry");
        ensure!(
            selected.same_pins(&fresh.entries[&number]),
            "Selected PR #{number} head, base branch, or identity changed; retry"
        );
        Ok(())
    }

    /// Parent-first tree, with deterministic sibling ordering and escaped metadata.
    pub fn render(&self) -> String {
        let mut text = String::new();
        let mut pending = vec![(self.root, 0)];
        while let Some((number, depth)) = pending.pop() {
            let entry = &self.entries[&number];
            let pr = &entry.pr;
            let marker = if number == self.current { "*" } else { "-" };
            let state = if entry.draft { "draft" } else { "ready" };
            writeln!(
                text,
                "{}{} #{} {} [{}; {}]{}",
                "  ".repeat(depth),
                marker,
                number,
                escaped(&pr.title),
                escaped(&entry.state),
                state,
                if number == self.current {
                    " (current)"
                } else {
                    ""
                }
            )
            .unwrap();
            writeln!(
                text,
                "{}  {}:{} -> {}:{}\n{}  {}",
                "  ".repeat(depth),
                escaped(&pr.head_repo),
                escaped(&pr.head_ref),
                escaped(&entry.base_repo),
                escaped(&entry.base_ref),
                "  ".repeat(depth),
                escaped(&pr.url)
            )
            .unwrap();
            pending.extend(
                self.child_numbers(number)
                    .iter()
                    .rev()
                    .map(|&child| (child, depth + 1)),
            );
        }
        text
    }

    fn child_numbers(&self, number: u64) -> &[u64] {
        self.children.get(&number).map_or(&[], Vec::as_slice)
    }

    fn from_listing(
        client: &Client,
        current: u64,
        reviewer: &str,
        values: Vec<Value>,
    ) -> Result<Self> {
        let mut entries = BTreeMap::new();
        let mut deleted = Vec::new();
        let mut seen = BTreeSet::new();
        for value in values {
            let parsed = parse_entry(client, reviewer, &value)?;
            let number = match &parsed {
                ListedEntry::Available(entry) => entry.pr.number,
                ListedEntry::Deleted(head) => head.number,
            };
            ensure!(
                seen.insert(number),
                "Duplicate PR #{number} in paginated stack discovery; retry"
            );
            match parsed {
                ListedEntry::Available(entry) => {
                    entries.insert(number, *entry);
                }
                ListedEntry::Deleted(head) => deleted.push(head),
            }
        }
        ensure!(
            entries.contains_key(&current),
            "Current PR is absent from the open PR listing or its head was deleted; retry"
        );
        let mut heads: BTreeMap<(&str, &str), Vec<u64>> = BTreeMap::new();
        for entry in entries.values() {
            heads
                .entry((&entry.pr.head_repo, &entry.pr.head_ref))
                .or_default()
                .push(entry.pr.number);
        }
        let mut parents: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for entry in entries.values() {
            if let Some(matches) = heads.get(&(entry.base_repo.as_str(), entry.base_ref.as_str())) {
                parents.insert(entry.pr.number, matches.clone());
                for parent in matches {
                    children.entry(*parent).or_default().push(entry.pr.number);
                }
            }
        }
        let mut component = BTreeSet::new();
        let mut pending = vec![current];
        while let Some(number) = pending.pop() {
            if component.insert(number) {
                pending.extend(parents.get(&number).into_iter().flatten().copied());
                pending.extend(children.get(&number).into_iter().flatten().copied());
            }
        }
        for &number in &component {
            let entry = &entries[&number];
            let candidates = parents.get(&number).map_or(&[][..], Vec::as_slice);
            ensure!(
                candidates.len() <= 1,
                "Ambiguous stack parent for PR #{number}; retry after resolving duplicate head branches"
            );
            for parent in candidates {
                ensure!(
                    entries[parent].pr.head_sha == entry.pr.base_sha,
                    "Stale stack relationship: parent PR #{parent} head does not equal PR #{number} base; retry"
                );
            }
            for head in &deleted {
                // A deleted repository cannot establish exact head identity. Do
                // not guess if its surviving ref could be this node's parent.
                ensure!(
                    !(head.base_repo == entry.pr.head_repo && head.base_ref == entry.pr.head_ref)
                        && head.head_ref != entry.base_ref,
                    "PR #{} has a deleted head and may be connected to this stack; cannot determine safe neighbors",
                    head.number
                );
            }
        }
        let parents: BTreeMap<u64, u64> = parents
            .into_iter()
            .filter(|(number, _)| component.contains(number))
            .map(|(number, candidates)| (number, candidates[0]))
            .collect();
        let mut root = current;
        let mut ancestors = BTreeSet::new();
        while let Some(&parent) = parents.get(&root) {
            ensure!(
                ancestors.insert(root),
                "Cycle in PR stack; no safe navigation is possible"
            );
            root = parent;
        }
        entries.retain(|number, _| component.contains(number));
        children.retain(|number, _| component.contains(number));
        Ok(Self {
            current,
            root,
            entries,
            parents,
            children,
        })
    }
}

struct DeletedHead {
    number: u64,
    head_ref: String,
    base_repo: String,
    base_ref: String,
}

enum ListedEntry {
    Available(Box<StackEntry>),
    Deleted(DeletedHead),
}

fn read_entry(client: &Client, number: u64, reviewer: &str) -> Result<StackEntry> {
    let value = client.get(&format!("repos/{}/pulls/{number}", client.repo))?;
    ensure!(
        value["number"].as_u64() == Some(number),
        "GitHub returned a different PR number"
    );
    match parse_entry(client, reviewer, &value)? {
        ListedEntry::Available(entry) => Ok(*entry),
        ListedEntry::Deleted(_) => {
            anyhow::bail!("PR #{number} head repository was deleted or is unavailable")
        }
    }
}

fn parse_entry(client: &Client, reviewer: &str, value: &Value) -> Result<ListedEntry> {
    let number = value["number"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("PR response is missing a positive PR number")?;
    ensure!(
        string(value, "state")? == "open"
            && value
                .get("merged")
                .is_none_or(|v| v.as_bool() == Some(false))
            && value.get("merged_at").is_none_or(Value::is_null),
        "PR #{number} must be open and not merged"
    );
    let url = string(value, "html_url")?;
    validate_url(url, client, number)?;
    let base_repo = string(&value["base"]["repo"], "full_name")?;
    validate_repo(base_repo)?;
    ensure!(
        base_repo.eq_ignore_ascii_case(&client.repo),
        "PR #{number} base repository does not match this client"
    );
    let base_ref = string(&value["base"], "ref")?;
    let head_ref = string(&value["head"], "ref")?;
    validate_ref(base_ref)?;
    validate_ref(head_ref)?;
    if value["head"].get("repo").is_some_and(Value::is_null) {
        return Ok(ListedEntry::Deleted(DeletedHead {
            number,
            head_ref: head_ref.into(),
            base_repo: base_repo.into(),
            base_ref: base_ref.into(),
        }));
    }
    let head_repo = string(&value["head"]["repo"], "full_name")?;
    validate_repo(head_repo)?;
    let head_sha = sha(&value["head"])?;
    let base_sha = sha(&value["base"])?;
    let author = string(&value["user"], "login")?;
    let login = author.strip_suffix("[bot]").unwrap_or(author);
    ensure!(
        login
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && login
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "Invalid PR author login"
    );
    Ok(ListedEntry::Available(Box::new(StackEntry {
        pr: PullRequest {
            host: client.host.to_ascii_lowercase(),
            repo: client.repo.clone(),
            number,
            url: url.into(),
            title: string(value, "title")?.into(),
            head_sha,
            base_sha,
            head_repo: head_repo.into(),
            head_ref: head_ref.into(),
            author: author.into(),
            reviewer: reviewer.into(),
        },
        base_repo: base_repo.into(),
        base_ref: base_ref.into(),
        state: "open".into(),
        draft: value["draft"]
            .as_bool()
            .context("PR response is missing a boolean draft field")?,
    })))
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("PR response is missing string field {key}"))
}

fn sha(value: &Value) -> Result<String> {
    let sha = string(value, "sha")?;
    ensure!(
        matches!(sha.len(), 40 | 64) && sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "Expected a full hexadecimal Git object ID (40 or 64 characters)"
    );
    Ok(sha.to_ascii_lowercase())
}

fn validate_repo(repo: &str) -> Result<()> {
    let (owner, name) = repo
        .split_once('/')
        .context("Repository must be owner/repo")?;
    ensure!(
        owner
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && owner
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && owner
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && !name.is_empty()
            && name != "."
            && name != ".."
            && !name.starts_with('-')
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
        "Invalid PR repository identity"
    );
    Ok(())
}

fn validate_ref(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != "@"
            && !name.starts_with('-')
            && !name.ends_with('.')
            && !name.contains("..")
            && !name.contains("@{")
            && !name
                .chars()
                .any(|c| c.is_control() || c == ' ' || "~^:?*[\\".contains(c))
            && !name
                .split('/')
                .any(|s| s.is_empty() || s.starts_with('.') || s.ends_with(".lock")),
        "Invalid PR branch ref"
    );
    Ok(())
}

fn validate_url(url: &str, client: &Client, number: u64) -> Result<()> {
    let parts: Vec<_> = url
        .strip_prefix("https://")
        .context("PR URL must use HTTPS")?
        .trim_end_matches('/')
        .split('/')
        .collect();
    ensure!(
        parts.len() == 5
            && parts[0].eq_ignore_ascii_case(&client.host)
            && format!("{}/{}", parts[1], parts[2]).eq_ignore_ascii_case(&client.repo)
            && parts[3] == "pull"
            && parts[4] == number.to_string(),
        "PR URL does not match its host, repository, and number"
    );
    Ok(())
}

fn escaped(value: &str) -> String {
    let mut output = String::new();
    for c in value.chars() {
        // Bidi formatting can disguise the surrounding terminal output too.
        if c.is_control()
            || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            output.extend(c.escape_default());
        } else {
            output.push(c);
        }
    }
    output
}
