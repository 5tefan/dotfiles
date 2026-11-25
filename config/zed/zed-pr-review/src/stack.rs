//! Read-only stacks defined solely by exact repository/branch relationships.
//!
//! A discovery checks a paginated listing against individual PR reads. GitHub does
//! not offer an atomic snapshot; callers must revalidate after a user chooses a PR.
//! Cached graphs live for at most 60 seconds. Their fast verifier checks only the
//! displayed edge, not the whole candidate set: additions (including new children)
//! are discovered on the next full refresh, after at most that cache window.

use crate::github::Client;
use crate::model::PullRequest;
use anyhow::{Context, Result, ensure};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const CACHE_VERSION: u32 = 1;
const CACHE_MAX_AGE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Deserialize, Serialize)]
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

// Edges and the current node are deliberately not stored: reconstructing them
// from exact branch identities validates the graph and allows safe re-centering.
#[derive(Deserialize, Serialize)]
struct CachedStack {
    version: u32,
    state: PathBuf,
    saved_at: SystemTime,
    entries: Vec<StackEntry>,
}

/// Owned, escaped lines shared by the live viewer and one-shot output.
#[derive(Clone, Debug)]
pub struct RenderedStack {
    pub lines: Vec<Line<'static>>,
    /// Zero-based index of the current PR's node/metadata line, not its title.
    pub current_row: usize,
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

    /// All other connected PRs, nearest first over undirected stack edges.
    /// At each node, enqueue children in PR-number order before its parent.
    pub fn preparation_order(&self) -> Vec<StackEntry> {
        let mut seen = BTreeSet::from([self.current]);
        let mut pending = VecDeque::from([self.current]);
        let mut entries = Vec::with_capacity(self.entries.len().saturating_sub(1));
        while let Some(number) = pending.pop_front() {
            for &neighbor in self
                .child_numbers(number)
                .iter()
                .chain(self.parents.get(&number))
            {
                if seen.insert(neighbor) {
                    entries.push(self.entries[&neighbor].clone());
                    pending.push_back(neighbor);
                }
            }
        }
        entries
    }

    /// Atomically remember a fully live discovery in an existing state directory.
    /// Call only after `discover`, never to extend the life of a cached graph.
    /// Takes no workspace lock and does not create or change a review draft.
    pub fn remember(&self, state: &Path) -> Result<()> {
        check_cache_state(state)?;
        let path = state.join("stack.json");
        check_cache_file(&path, true)?;
        let cached = CachedStack {
            version: CACHE_VERSION,
            state: state.to_path_buf(),
            saved_at: SystemTime::now(),
            entries: self.entries.values().cloned().collect(),
        };
        let mut temporary = tempfile::NamedTempFile::new_in(state)?;
        serde_json::to_writer_pretty(&mut temporary, &cached)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(state)?.sync_all()?;
        Ok(())
    }

    /// Read a graph no more than 60 seconds old, centered on these exact PR pins.
    /// The canonical state path binds the cache to its workspace. Missing, stale,
    /// corrupt, relocated, or aliased state is a read-only miss. No GitHub calls.
    /// Before navigation, callers must still run `verify_cached_adjacent`.
    pub fn cached(state: &Path, current: &PullRequest) -> Option<Self> {
        Self::load_cached(state, current).ok()
    }

    fn load_cached(state: &Path, current: &PullRequest) -> Result<Self> {
        check_cache_state(state)?;
        let path = state.join("stack.json");
        check_cache_file(&path, false)?;
        let cached: CachedStack = serde_json::from_slice(&fs::read(&path)?)?;
        ensure!(
            cached.version == CACHE_VERSION,
            "Unsupported stack cache version"
        );
        ensure!(
            cached.state.as_os_str() == state.as_os_str(),
            "Stack cache belongs to a different state directory"
        );
        ensure!(
            SystemTime::now().duration_since(cached.saved_at)? <= CACHE_MAX_AGE,
            "Stack cache expired"
        );
        let mut entries = BTreeMap::new();
        for entry in cached.entries {
            ensure!(
                entry.pr.host == current.host
                    && entry.pr.repo == current.repo
                    && entry.pr.reviewer == current.reviewer
                    && entry.pr.number > 0
                    && entry.state == "open",
                "Invalid cached PR identity or state"
            );
            validate_repo(&entry.pr.repo)?;
            validate_repo(&entry.pr.head_repo)?;
            validate_repo(&entry.base_repo)?;
            ensure!(
                entry.base_repo.eq_ignore_ascii_case(&entry.pr.repo),
                "Invalid cached base repository"
            );
            validate_ref(&entry.pr.head_ref)?;
            validate_ref(&entry.base_ref)?;
            validate_sha(&entry.pr.head_sha)?;
            validate_sha(&entry.pr.base_sha)?;
            validate_author(&entry.pr.author)?;
            validate_url(
                &entry.pr.url,
                &entry.pr.host,
                &entry.pr.repo,
                entry.pr.number,
            )?;
            ensure!(
                entries.insert(entry.pr.number, entry).is_none(),
                "Duplicate PR in stack cache"
            );
        }
        ensure!(
            entries
                .get(&current.number)
                .is_some_and(|entry| entry.matches_pr(current)),
            "Current PR pins do not match stack cache"
        );
        let count = entries.len();
        let stack = Self::from_entries(current.number, entries, Vec::new())?;
        ensure!(stack.entries.len() == count, "Disconnected stack cache");
        Ok(stack)
    }

    /// Recheck only the current PR, the selected PR, and the reviewer live.
    /// Both PRs must retain their cached pins and exact displayed relationship.
    /// Unlike `verify_adjacent`, this does not list PRs or check other nodes:
    /// new children/ambiguous candidates wait for the next full refresh (<=60s).
    /// Run immediately before opening the target, after any selection prompt.
    pub fn verify_cached_adjacent(&self, client: &Client, target: &PullRequest) -> Result<()> {
        let number = target.number;
        let was_parent = self.parents.get(&self.current) == Some(&number);
        ensure!(
            was_parent || self.child_numbers(self.current).contains(&number),
            "PR #{number} is not an adjacent stack candidate"
        );
        let selected = &self.entries[&number];
        ensure!(
            selected.matches_pr(target),
            "Selected PR #{number} changed since stack discovery; retry"
        );
        let current = self.current();
        ensure!(
            client.host == current.pr.host
                && client.repo == current.pr.repo
                && client.reviewer == current.pr.reviewer,
            "Current PR host, repository, or reviewer does not match this client"
        );
        let reviewer = client.authenticated_reviewer()?;
        ensure!(
            reviewer == current.pr.reviewer,
            "The pinned reviewer changed"
        );
        let fresh_current = read_entry(client, self.current, &reviewer)?;
        ensure!(
            current.same_pins(&fresh_current),
            "Current PR head, base branch, or identity changed since stack discovery; retry"
        );
        let fresh_target = read_entry(client, number, &reviewer)?;
        ensure!(
            selected.same_pins(&fresh_target) && fresh_target.matches_pr(target),
            "Selected PR #{number} head, base branch, or identity changed; retry"
        );
        let (parent, child) = if was_parent {
            (&fresh_target, &fresh_current)
        } else {
            (&fresh_current, &fresh_target)
        };
        ensure!(
            parent.pr.head_repo == child.base_repo
                && parent.pr.head_ref == child.base_ref
                && parent.pr.head_sha == child.pr.base_sha,
            "Selected stack relationship changed; retry"
        );
        ensure!(
            client.authenticated_reviewer()? == reviewer,
            "The pinned reviewer changed during stack verification"
        );
        Ok(())
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

    /// Children-first graph with PR-number ordering between sibling subtrees.
    /// Linear stacks use one column; extra lanes hold edges to pending parents.
    pub fn render_log(&self) -> RenderedStack {
        let mut rendered = RenderedStack {
            lines: Vec::new(),
            current_row: 0,
        };
        let dim = Style::default().add_modifier(Modifier::DIM);
        let current_style = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD);
        let branch_style = Style::default().fg(Color::Cyan);
        let root = &self.entries[&self.root];
        let mut lanes = Vec::new();
        // Iterative postorder avoids recursion and depth-dependent indentation.
        // With one parent per node, the active node is always in the rightmost
        // lane. Other lanes are edges from earlier subtrees to pending ancestors.
        let mut pending = vec![(self.root, false)];
        while let Some((number, visited)) = pending.pop() {
            if !visited {
                pending.push((number, true));
                pending.extend(
                    self.child_numbers(number)
                        .iter()
                        .rev()
                        .map(|&child| (child, false)),
                );
                continue;
            }
            if lanes.last() != Some(&number) {
                lanes.push(number);
            }
            let column = lanes.len() - 1;
            let entry = &self.entries[&number];
            let pr = &entry.pr;
            let is_current = number == self.current;
            if is_current {
                rendered.current_row = rendered.lines.len();
            }
            let mut metadata = vec![
                Span::styled("│ ".repeat(column), dim),
                Span::styled(
                    if is_current { "@" } else { "○" },
                    if is_current { current_style } else { dim },
                ),
                Span::raw(" "),
                Span::styled(format!("#{number}"), Style::default().fg(Color::Magenta)),
                Span::raw(" "),
                Span::styled(
                    branch_label(&pr.head_repo, &pr.head_ref, pr.head_repo != root.base_repo),
                    branch_style,
                ),
                Span::raw(" "),
                Span::styled(
                    if entry.draft { "[draft]" } else { "[ready]" },
                    Style::default().fg(if entry.draft {
                        Color::Yellow
                    } else {
                        Color::Green
                    }),
                ),
            ];
            if is_current {
                metadata.push(Span::styled(" (current)", current_style));
            }
            rendered.lines.push(Line::from(metadata));
            let connector = "│ ".repeat(lanes.len());
            rendered.lines.push(Line::from(vec![
                Span::styled(connector.clone(), dim),
                Span::raw(escaped(&pr.title)),
            ]));
            rendered.lines.push(Line::from(vec![
                Span::styled(connector, dim),
                Span::styled(escaped(&pr.url), dim),
            ]));
            if let Some(&parent) = self.parents.get(&number) {
                if column > 0 && lanes[column - 1] == parent {
                    // Join only the two edges that share this parent. Earlier
                    // lanes pass through unchanged, including at nested forks.
                    rendered
                        .lines
                        .push(Line::styled(format!("{}├─╯", "│ ".repeat(column - 1)), dim));
                    lanes.pop();
                } else {
                    lanes[column] = parent;
                }
            }
        }
        let cross_repo = self
            .entries
            .values()
            .any(|entry| entry.pr.head_repo != root.base_repo);
        rendered.lines.push(Line::from(vec![
            Span::styled("◆ ", dim),
            Span::styled(
                branch_label(&root.base_repo, &root.base_ref, cross_repo),
                branch_style,
            ),
            Span::styled(" (base)", dim),
        ]));
        rendered
    }

    /// The same graph as `render_log`, without terminal escape sequences.
    pub fn render(&self) -> String {
        let mut text = String::new();
        for line in self.render_log().lines {
            writeln!(text, "{line}").unwrap();
        }
        text
    }

    /// ANSI output for a one-shot TTY display. Use `render` for redirected output.
    pub fn render_colored(&self) -> String {
        let mut text = String::new();
        for line in self.render_log().lines {
            for span in line.spans {
                write_log_style(&mut text, line.style.patch(span.style));
                text.push_str(&span.content);
            }
            text.push_str("\x1b[0m\n");
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
        Self::from_entries(current, entries, deleted)
    }

    fn from_entries(
        current: u64,
        mut entries: BTreeMap<u64, StackEntry>,
        deleted: Vec<DeletedHead>,
    ) -> Result<Self> {
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

fn check_cache_state(state: &Path) -> Result<()> {
    ensure!(
        state.is_absolute() && state.canonicalize()?.as_os_str() == state.as_os_str(),
        "Stack state directory must use its canonical path"
    );
    for path in state.ancestors() {
        ensure!(
            fs::symlink_metadata(path)?.is_dir(),
            "Stack state ancestors must be directories, not symlinks"
        );
        check_exact_name(path)?;
    }
    Ok(())
}

fn check_exact_name(path: &Path) -> Result<()> {
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        let exact = fs::read_dir(parent)?.try_fold(false, |found, entry| {
            Ok::<_, std::io::Error>(found || entry?.file_name() == name)
        })?;
        ensure!(exact, "Stack state path is missing or aliased");
    }
    Ok(())
}

fn check_cache_file(path: &Path, allow_missing: bool) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file(),
        "Stack cache must be a regular file, not a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.nlink() == 1,
            "Hard-linked stack cache is not allowed"
        );
    }
    check_exact_name(path)?;
    ensure!(
        path.canonicalize()?.as_os_str() == path.as_os_str(),
        "Stack cache path is an alias"
    );
    Ok(())
}

fn branch_label(repo: &str, branch: &str, qualified: bool) -> String {
    if qualified {
        format!("{}:{}", escaped(repo), escaped(branch))
    } else {
        escaped(branch)
    }
}

// The log uses foreground colors, bold and dim only. Reset before each span
// and at each line end so styles cannot leak into titles or the caller's output.
fn write_log_style(text: &mut String, style: Style) {
    text.push_str("\x1b[0");
    if style.add_modifier.contains(Modifier::BOLD) {
        text.push_str(";1");
    }
    if style.add_modifier.contains(Modifier::DIM) {
        text.push_str(";2");
    }
    if let Some(color) = style.fg {
        let code = match color {
            Color::Reset => 39,
            Color::Black => 30,
            Color::Red => 31,
            Color::Green => 32,
            Color::Yellow => 33,
            Color::Blue => 34,
            Color::Magenta => 35,
            Color::Cyan => 36,
            Color::Gray => 37,
            Color::DarkGray => 90,
            Color::LightRed => 91,
            Color::LightGreen => 92,
            Color::LightYellow => 93,
            Color::LightBlue => 94,
            Color::LightMagenta => 95,
            Color::LightCyan => 96,
            Color::White => 97,
            Color::Indexed(index) => {
                write!(text, ";38;5;{index}").unwrap();
                39
            }
            Color::Rgb(r, g, b) => {
                write!(text, ";38;2;{r};{g};{b}").unwrap();
                39
            }
        };
        if !matches!(color, Color::Indexed(_) | Color::Rgb(..)) {
            write!(text, ";{code}").unwrap();
        }
    }
    text.push('m');
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
    validate_url(url, &client.host, &client.repo, number)?;
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
    validate_author(author)?;
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
    validate_sha(sha)?;
    Ok(sha.to_ascii_lowercase())
}

fn validate_sha(sha: &str) -> Result<()> {
    ensure!(
        matches!(sha.len(), 40 | 64) && sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "Expected a full hexadecimal Git object ID (40 or 64 characters)"
    );
    Ok(())
}

fn validate_author(author: &str) -> Result<()> {
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
    Ok(())
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

fn validate_url(url: &str, host: &str, repo: &str, number: u64) -> Result<()> {
    let parts: Vec<_> = url
        .strip_prefix("https://")
        .context("PR URL must use HTTPS")?
        .trim_end_matches('/')
        .split('/')
        .collect();
    ensure!(
        parts.len() == 5
            && parts[0].eq_ignore_ascii_case(host)
            && format!("{}/{}", parts[1], parts[2]).eq_ignore_ascii_case(repo)
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
