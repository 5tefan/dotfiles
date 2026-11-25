//! Push a `jj` bookmark stack to `origin` and create/retarget GitHub PRs in topological order.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::github::{BranchPr, BranchPrState, Client};
use crate::review_stack::BOOKMARK_PREFIX;

const INTERNAL_BOOKMARK_PREFIX: &str = "zed-pr-review/";
const MAX_TITLE_CHARS: usize = 70;

const COMMIT_TEMPLATE: &str = r#"json({
    "commit_id" => commit_id,
    "change_id" => change_id,
    "parents" => parents.map(|p| p.commit_id()),
    "description" => description,
    "conflict" => conflict,
    "immutable" => immutable,
    "local_bookmark_names" => local_bookmarks.map(|b| b.name()),
    "local_bookmark_conflicts" => local_bookmarks.map(|b| b.conflict()),
}) ++ "\n""#;

const ORIGIN_BOOKMARK_TEMPLATE: &str = r#"if(remote == "origin" && present, name ++ "\t" ++ if(normal_target, normal_target.commit_id(), "") ++ "\t" ++ conflict ++ "\n")"#;

#[derive(Clone, Debug, Default)]
pub struct PushOptions {
    pub revset: Option<String>,
    pub dry_run: bool,
    pub json: bool,
    pub update_title: bool,
    pub open: bool,
    pub force_new_pr: bool,
    pub ready: bool,
    pub allow_empty_description: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncAction {
    Created,
    Retargeted,
    Reopened,
    Unchanged,
}

impl SyncAction {
    fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Retargeted => "retargeted",
            Self::Reopened => "reopened",
            Self::Unchanged => "unchanged",
        }
    }

    fn dry_run_label(self) -> &'static str {
        match self {
            Self::Created => "would create",
            Self::Retargeted => "would retarget",
            Self::Reopened => "would reopen",
            Self::Unchanged => "unchanged",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PushedBookmark {
    pub bookmark: String,
    pub base: String,
    pub pr_number: Option<u64>,
    pub url: Option<String>,
    pub head_sha: String,
    pub title: String,
    pub draft: bool,
    pub action: SyncAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PushReport {
    pub host: String,
    pub repo: String,
    pub dry_run: bool,
    pub stack: Vec<PushedBookmark>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BookmarkRef {
    name: String,
    conflict: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommitInfo {
    commit_id: String,
    change_id: String,
    parents: Vec<String>,
    description: String,
    conflict: bool,
    immutable: bool,
    local_bookmarks: Vec<BookmarkRef>,
}

#[derive(Deserialize)]
struct RawCommitInfo {
    commit_id: String,
    change_id: String,
    parents: Vec<String>,
    description: String,
    conflict: bool,
    immutable: bool,
    local_bookmark_names: Vec<String>,
    local_bookmark_conflicts: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteBookmark {
    target: String,
    conflict: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedBookmark {
    bookmark: String,
    base: String,
    commit_id: String,
    title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PrDecision {
    Create {
        draft: bool,
    },
    Retarget {
        pr: BranchPr,
        new_base: Option<String>,
        new_title: Option<String>,
        mark_ready: bool,
    },
    Reopen {
        pr: BranchPr,
        new_base: Option<String>,
        new_title: Option<String>,
        mark_ready: bool,
    },
    Unchanged {
        pr: BranchPr,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StackBookmark {
    pub bookmark: String,
    pub commit_id: String,
    pub change_id: String,
    pub description_first_line: String,
    pub remote_target: Option<String>,
}

/// Discover the current jj stack's eligible local bookmarks in bottom-to-top
/// topological order without rejecting empty descriptions.
pub(crate) fn discover_stack_bookmarks(
    root: &Path,
    git_dir: &Path,
    revset: Option<&str>,
    ignore_wc: bool,
) -> Result<Vec<StackBookmark>> {
    let origin_bookmarks = read_origin_bookmarks(root, ignore_wc)?;
    let trunk_bookmark = resolve_trunk_bookmark(root, git_dir, &origin_bookmarks)?;

    let (all_commits, selected_ids) = if let Some(revset) = revset {
        let selected_commits = read_commits(root, revset, ignore_wc)?;
        ensure!(
            !selected_commits.is_empty(),
            "Revset `{revset}` did not match any revisions"
        );
        let selected_ids: HashSet<String> = selected_commits
            .iter()
            .map(|c| c.commit_id.clone())
            .collect();
        let graph_revset = format!("trunk()..({revset}) | ({revset})");
        let graph_commits = read_commits(root, &graph_revset, true)?;
        let mut by_id: HashMap<String, CommitInfo> = graph_commits
            .into_iter()
            .map(|c| (c.commit_id.clone(), c))
            .collect();
        for c in selected_commits {
            by_id.entry(c.commit_id.clone()).or_insert(c);
        }
        (by_id, selected_ids)
    } else {
        let up_to_at: HashSet<String> = read_commits(root, "trunk()..@ & mutable()", ignore_wc)?
            .into_iter()
            .map(|c| c.commit_id)
            .collect();
        let descendants_of_at: HashSet<String> = read_commits(root, "(@:: ~ @) & mutable()", true)?
            .into_iter()
            .map(|c| c.commit_id)
            .collect();
        let connected = read_commits(root, "roots(trunk()..@):: & mutable()", true)?;
        let by_id: HashMap<String, CommitInfo> = connected
            .into_iter()
            .map(|c| (c.commit_id.clone(), c))
            .collect();
        let selected_ids = select_default_commits(
            &by_id,
            &up_to_at,
            &descendants_of_at,
            &origin_bookmarks,
            &trunk_bookmark,
        );
        (by_id, selected_ids)
    };

    let mut commit_bookmarks: HashMap<String, Vec<String>> = HashMap::new();
    let mut commit_sort_key: HashMap<String, String> = HashMap::new();
    for id in &selected_ids {
        let Some(commit) = all_commits.get(id) else {
            continue;
        };
        let mut names: Vec<String> = eligible_bookmarks(commit, &trunk_bookmark)
            .into_iter()
            .filter(|b| !b.conflict)
            .map(|b| b.name.clone())
            .collect();
        names.sort();
        if let Some(first) = names.first() {
            commit_sort_key.insert(id.clone(), first.clone());
            commit_bookmarks.insert(id.clone(), names);
        }
    }

    if commit_sort_key.is_empty() {
        return Ok(Vec::new());
    }

    let ordered_ids = topo_sort_selected(&commit_sort_key, &all_commits)?;
    let mut result = Vec::new();
    for id in ordered_ids {
        let commit = &all_commits[&id];
        let first_line = commit
            .description
            .lines()
            .next()
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        if let Some(names) = commit_bookmarks.get(&id) {
            for name in names {
                let remote_target = origin_bookmarks
                    .get(name)
                    .filter(|r| !r.conflict && !r.target.is_empty())
                    .map(|r| r.target.clone());
                result.push(StackBookmark {
                    bookmark: name.clone(),
                    commit_id: commit.commit_id.clone(),
                    change_id: commit.change_id.clone(),
                    description_first_line: first_line.clone(),
                    remote_target,
                });
            }
        }
    }
    Ok(result)
}

/// Map local eligible bookmark names to their current local commit IDs,
/// filtering to bookmarks that also exist on `origin`.
pub(crate) fn local_pushed_bookmarks(root: &Path, git_dir: &Path) -> Result<Vec<(String, String)>> {
    let origin_bookmarks = read_origin_bookmarks(root, true)?;
    let trunk_bookmark = resolve_trunk_bookmark(root, git_dir, &origin_bookmarks)?;
    let commits = read_commits(root, "bookmarks() & mutable()", true)?;
    let mut result = Vec::new();
    for commit in commits {
        for bm in eligible_bookmarks(&commit, &trunk_bookmark) {
            if !bm.conflict && origin_bookmarks.contains_key(&bm.name) {
                result.push((bm.name.clone(), commit.commit_id.clone()));
            }
        }
    }
    Ok(result)
}

pub fn run(client: &Client, opts: &PushOptions) -> Result<PushReport> {
    let ignore_wc = opts.dry_run;
    ensure_not_review_stack(&client.root, ignore_wc)?;

    let origin_bookmarks = read_origin_bookmarks(&client.root, true)?;
    let trunk_bookmark = resolve_trunk_bookmark(&client.root, &client.git_dir, &origin_bookmarks)?;

    let plan = build_plan(&client.root, opts, &origin_bookmarks, &trunk_bookmark)?;

    // Check existing PRs and ownership BEFORE pushing or mutating anything.
    let mut decisions: Vec<(PlannedBookmark, PrDecision)> = Vec::with_capacity(plan.len());
    for item in plan {
        let prs = client.list_prs_for_head(&item.bookmark)?;
        let decision = decide_pr_action(
            &item,
            &prs,
            &client.reviewer,
            opts.update_title,
            opts.force_new_pr,
            opts.ready,
        )?;
        decisions.push((item, decision));
    }

    if !opts.dry_run {
        for (item, _) in &decisions {
            if !opts.json {
                println!("Pushing {} to origin", item.bookmark);
            }
            push_bookmark(&client.root, &item.bookmark, opts.allow_empty_description)?;
        }

        // Verify remote refs are up to date before creating or retargeting PRs.
        let updated_origin = read_origin_bookmarks(&client.root, true)?;
        verify_remote_refs(&decisions, &updated_origin)?;
    }

    let default_body = if decisions
        .iter()
        .any(|(_, decision)| matches!(decision, PrDecision::Create { .. }))
    {
        read_pr_template(&client.root)?
    } else {
        String::new()
    };

    let mut stack = Vec::with_capacity(decisions.len());
    let mut urls_to_open = Vec::new();

    for (item, decision) in decisions {
        let entry = if opts.dry_run {
            dry_run_entry(&item, &decision)
        } else {
            execute_decision(client, &item, &decision, &default_body)?
        };
        if let Some(url) = &entry.url {
            urls_to_open.push(url.clone());
        }
        if !opts.json {
            print_entry(&entry, opts.dry_run);
        }
        stack.push(entry);
    }

    let report = PushReport {
        host: client.host.clone(),
        repo: client.repo.clone(),
        dry_run: opts.dry_run,
        stack,
    };

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("Cannot serialize push report")?
        );
    }

    if opts.open && !opts.dry_run {
        for url in &urls_to_open {
            open_url(url)?;
        }
    }

    Ok(report)
}

fn build_plan(
    root: &Path,
    opts: &PushOptions,
    origin_bookmarks: &BTreeMap<String, RemoteBookmark>,
    trunk_bookmark: &str,
) -> Result<Vec<PlannedBookmark>> {
    let (all_commits, selected_ids) = if let Some(revset) = &opts.revset {
        let selected_commits = read_commits(root, revset, true)?;
        ensure!(
            !selected_commits.is_empty(),
            "Revset `{revset}` did not match any revisions"
        );
        let selected_ids: HashSet<String> = selected_commits
            .iter()
            .map(|c| c.commit_id.clone())
            .collect();
        // Load all non-trunk ancestors of selected commits so base resolution
        // stops cleanly at `::trunk()`.
        let graph_revset = format!("trunk()..({revset}) | ({revset})");
        let graph_commits = read_commits(root, &graph_revset, true)?;
        let mut by_id: HashMap<String, CommitInfo> = graph_commits
            .into_iter()
            .map(|c| (c.commit_id.clone(), c))
            .collect();
        for c in selected_commits {
            by_id.entry(c.commit_id.clone()).or_insert(c);
        }
        (by_id, selected_ids)
    } else {
        let up_to_at: HashSet<String> = read_commits(root, "trunk()..@ & mutable()", true)?
            .into_iter()
            .map(|c| c.commit_id)
            .collect();
        let descendants_of_at: HashSet<String> = read_commits(root, "(@:: ~ @) & mutable()", true)?
            .into_iter()
            .map(|c| c.commit_id)
            .collect();
        let connected = read_commits(root, "roots(trunk()..@):: & mutable()", true)?;
        let by_id: HashMap<String, CommitInfo> = connected
            .into_iter()
            .map(|c| (c.commit_id.clone(), c))
            .collect();
        let selected_ids = select_default_commits(
            &by_id,
            &up_to_at,
            &descendants_of_at,
            origin_bookmarks,
            trunk_bookmark,
        );
        (by_id, selected_ids)
    };

    plan_from_graph(
        &all_commits,
        &selected_ids,
        origin_bookmarks,
        trunk_bookmark,
        opts.allow_empty_description,
    )
}

fn is_eligible_bookmark(name: &str, trunk_bookmark: &str) -> bool {
    !name.starts_with(BOOKMARK_PREFIX)
        && !name.starts_with(INTERNAL_BOOKMARK_PREFIX)
        && name != trunk_bookmark
}

fn eligible_bookmarks<'a>(commit: &'a CommitInfo, trunk_bookmark: &str) -> Vec<&'a BookmarkRef> {
    commit
        .local_bookmarks
        .iter()
        .filter(|b| is_eligible_bookmark(&b.name, trunk_bookmark))
        .collect()
}

fn select_default_commits(
    commits: &HashMap<String, CommitInfo>,
    up_to_at: &HashSet<String>,
    descendants_of_at: &HashSet<String>,
    origin_bookmarks: &BTreeMap<String, RemoteBookmark>,
    trunk_bookmark: &str,
) -> HashSet<String> {
    let mut selected = HashSet::new();
    for (id, commit) in commits {
        let bms = eligible_bookmarks(commit, trunk_bookmark);
        if bms.is_empty() {
            continue;
        }
        if up_to_at.contains(id) {
            selected.insert(id.clone());
        } else if descendants_of_at.contains(id)
            && bms.iter().any(|b| origin_bookmarks.contains_key(&b.name))
        {
            selected.insert(id.clone());
        }
    }

    // Ancestor closure: if an unpushed bookmarked commit sits between `@` and
    // an already-pushed descendant, include it so the child PR's base exists on origin.
    let mut queue: VecDeque<String> = selected.iter().cloned().collect();
    let mut visited: HashSet<String> = selected.clone();
    while let Some(id) = queue.pop_front() {
        if let Some(commit) = commits.get(&id) {
            for parent_id in &commit.parents {
                if !visited.insert(parent_id.clone()) {
                    continue;
                }
                if let Some(parent) = commits.get(parent_id) {
                    if !eligible_bookmarks(parent, trunk_bookmark).is_empty() {
                        selected.insert(parent_id.clone());
                    }
                    queue.push_back(parent_id.clone());
                }
            }
        }
    }

    selected
}

fn plan_from_graph(
    commits: &HashMap<String, CommitInfo>,
    selected_ids: &HashSet<String>,
    origin_bookmarks: &BTreeMap<String, RemoteBookmark>,
    trunk_bookmark: &str,
    allow_empty_description: bool,
) -> Result<Vec<PlannedBookmark>> {
    // Validate selected commits and extract their single local bookmark.
    let mut commit_bookmark: HashMap<String, String> = HashMap::new();
    let mut selected_bookmark_names: HashSet<String> = HashSet::new();

    for id in selected_ids {
        let commit = commits
            .get(id)
            .with_context(|| format!("Selected commit {id} missing from graph"))?;
        let bms = eligible_bookmarks(commit, trunk_bookmark);
        if bms.is_empty() {
            continue;
        }
        ensure!(
            bms.len() == 1,
            "Revision {} has multiple local bookmarks ({}); parent/child base is ambiguous",
            short_id(&commit.change_id),
            bms.iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let bm = bms[0];
        ensure!(
            !bm.conflict,
            "Bookmark `{}` is conflicted; resolve the bookmark conflict before pushing",
            bm.name
        );
        ensure!(
            !commit.conflict,
            "Bookmark `{}` points to a conflicted revision ({}); resolve conflicts before pushing",
            bm.name,
            short_id(&commit.change_id)
        );
        if !allow_empty_description {
            ensure!(
                !commit.description.trim().is_empty(),
                "Bookmark `{}` has an empty description (describe it first or pass --allow-empty-description)",
                bm.name
            );
        }
        commit_bookmark.insert(id.clone(), bm.name.clone());
        selected_bookmark_names.insert(bm.name.clone());
    }

    ensure!(
        !commit_bookmark.is_empty(),
        "No pushable local bookmarks found in scope"
    );

    // Resolve the base branch for each selected bookmark.
    let mut bases: HashMap<String, String> = HashMap::new();
    for (id, bm_name) in &commit_bookmark {
        let base = resolve_base(id, bm_name, commits, trunk_bookmark)?;
        if base != trunk_bookmark
            && !selected_bookmark_names.contains(&base)
            && !origin_bookmarks.contains_key(&base)
        {
            bail!(
                "Base bookmark `{base}` for `{bm_name}` is not in the push scope and does not exist on origin; include `{base}` or push it first"
            );
        }
        bases.insert(id.clone(), base);
    }

    // Topological sort (Kahn's algorithm) strictly bottom-to-top.
    let ordered_ids = topo_sort_selected(&commit_bookmark, commits)?;

    let mut planned = Vec::with_capacity(ordered_ids.len());
    for id in ordered_ids {
        let commit = &commits[&id];
        let bookmark = commit_bookmark[&id].clone();
        let base = bases[&id].clone();
        let title = format_pr_title(&commit.description, &bookmark);
        planned.push(PlannedBookmark {
            bookmark,
            base,
            commit_id: commit.commit_id.clone(),
            title,
        });
    }

    Ok(planned)
}

/// Find the nearest ancestor bookmark strictly above `::trunk()`.
/// Commits in `::trunk()` are absent from `commits`, so any path reaching
/// `::trunk()` terminates cleanly and falls back to `trunk_bookmark` — even
/// when `main` moved ahead and this stack has not been rebased yet.
fn resolve_base(
    start_id: &str,
    bm_name: &str,
    commits: &HashMap<String, CommitInfo>,
    trunk_bookmark: &str,
) -> Result<String> {
    let start = &commits[start_id];
    let mut queue: VecDeque<String> = start.parents.iter().cloned().collect();
    let mut visited: HashSet<String> = HashSet::new();
    let mut found_bases: BTreeSet<String> = BTreeSet::new();

    while let Some(id) = queue.pop_front() {
        if !visited.insert(id.clone()) {
            continue;
        }
        let Some(commit) = commits.get(&id) else {
            // Reached a commit in `::trunk()` (or root).
            continue;
        };
        if commit.immutable {
            // Do not pick up stale bookmarks on immutable history.
            continue;
        }
        // If `trunk_bookmark` is explicitly placed on a mutable ancestor, treat it as the base.
        if commit
            .local_bookmarks
            .iter()
            .any(|b| b.name == trunk_bookmark)
        {
            found_bases.insert(trunk_bookmark.to_owned());
            continue;
        }
        let bms = eligible_bookmarks(commit, trunk_bookmark);
        if bms.len() > 1 {
            bail!(
                "Ancestor revision {} of `{bm_name}` has multiple local bookmarks ({}); base branch is ambiguous",
                short_id(&commit.change_id),
                bms.iter()
                    .map(|b| b.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(bm) = bms.first() {
            found_bases.insert(bm.name.clone());
            // Do not traverse past the nearest ancestor bookmark on this path.
            continue;
        }
        for parent_id in &commit.parents {
            queue.push_back(parent_id.clone());
        }
    }

    match found_bases.len() {
        0 => Ok(trunk_bookmark.to_owned()),
        1 => Ok(found_bases.into_iter().next().unwrap()),
        _ => bail!(
            "Bookmark `{bm_name}` has multiple nearest ancestor bookmarks ({}); base branch is ambiguous",
            found_bases.into_iter().collect::<Vec<_>>().join(", ")
        ),
    }
}

fn topo_sort_selected(
    commit_bookmark: &HashMap<String, String>,
    commits: &HashMap<String, CommitInfo>,
) -> Result<Vec<String>> {
    // Build parent -> child edges among selected commits by walking through
    // unbookmarked intermediate commits.
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut indegree: HashMap<String, usize> =
        commit_bookmark.keys().map(|id| (id.clone(), 0)).collect();

    for id in commit_bookmark.keys() {
        let mut queue: VecDeque<String> = commits[id].parents.iter().cloned().collect();
        let mut visited: HashSet<String> = HashSet::new();
        let mut direct_selected_parents: HashSet<String> = HashSet::new();

        while let Some(parent_id) = queue.pop_front() {
            if !visited.insert(parent_id.clone()) {
                continue;
            }
            if commit_bookmark.contains_key(&parent_id) {
                direct_selected_parents.insert(parent_id);
                continue;
            }
            if let Some(parent_commit) = commits.get(&parent_id) {
                for gp in &parent_commit.parents {
                    queue.push_back(gp.clone());
                }
            }
        }

        for parent_id in direct_selected_parents {
            children.entry(parent_id).or_default().push(id.clone());
            *indegree.get_mut(id).unwrap() += 1;
        }
    }

    // Deterministic Kahn's algorithm ordered by bookmark name.
    let mut ready: BTreeSet<(String, String)> = indegree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(id, _)| (commit_bookmark[id].clone(), id.clone()))
        .collect();

    let mut ordered = Vec::with_capacity(commit_bookmark.len());
    while let Some((_, id)) = ready.pop_first() {
        ordered.push(id.clone());
        if let Some(kids) = children.get(&id) {
            for kid in kids {
                let deg = indegree.get_mut(kid).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    ready.insert((commit_bookmark[kid].clone(), kid.clone()));
                }
            }
        }
    }

    ensure!(
        ordered.len() == commit_bookmark.len(),
        "Cycle detected while ordering bookmark stack"
    );
    Ok(ordered)
}

fn format_pr_title(description: &str, bookmark: &str) -> String {
    let first_line = description.trim().lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        bookmark.to_owned()
    } else {
        first_line.chars().take(MAX_TITLE_CHARS).collect()
    }
}

fn select_existing_pr<'a>(prs: &'a [BranchPr], head: &str) -> Option<&'a BranchPr> {
    let matching: Vec<&BranchPr> = prs.iter().filter(|pr| pr.head_ref == head).collect();
    // Prefer open PR, then most recent closed (unmerged) PR, then most recent merged PR.
    if let Some(open) = matching
        .iter()
        .filter(|pr| pr.state == BranchPrState::Open)
        .max_by_key(|pr| pr.number)
    {
        return Some(*open);
    }
    if let Some(closed) = matching
        .iter()
        .filter(|pr| pr.state == BranchPrState::Closed)
        .max_by_key(|pr| pr.number)
    {
        return Some(*closed);
    }
    matching
        .into_iter()
        .filter(|pr| pr.state == BranchPrState::Merged)
        .max_by_key(|pr| pr.number)
}

fn decide_pr_action(
    item: &PlannedBookmark,
    prs: &[BranchPr],
    current_user: &str,
    update_title: bool,
    force_new_pr: bool,
    ready: bool,
) -> Result<PrDecision> {
    let existing = if force_new_pr {
        None
    } else {
        select_existing_pr(prs, &item.bookmark)
    };

    let Some(pr) = existing else {
        return Ok(PrDecision::Create { draft: !ready });
    };

    if !pr.author.is_empty() && !pr.author.eq_ignore_ascii_case(current_user) {
        bail!(
            "PR #{} ({}) is owned by @{}, not @{}; refusing to modify",
            pr.number,
            item.bookmark,
            pr.author,
            current_user
        );
    }

    match pr.state {
        BranchPrState::Open => {
            let new_base = (pr.base_ref != item.base).then(|| item.base.clone());
            let new_title = (update_title && pr.title != item.title).then(|| item.title.clone());
            let mark_ready = ready && pr.draft;
            if new_base.is_none() && new_title.is_none() && !mark_ready {
                Ok(PrDecision::Unchanged { pr: pr.clone() })
            } else {
                Ok(PrDecision::Retarget {
                    pr: pr.clone(),
                    new_base,
                    new_title,
                    mark_ready,
                })
            }
        }
        BranchPrState::Closed => {
            let new_base = (pr.base_ref != item.base).then(|| item.base.clone());
            let new_title = (update_title && pr.title != item.title).then(|| item.title.clone());
            let mark_ready = ready && pr.draft;
            Ok(PrDecision::Reopen {
                pr: pr.clone(),
                new_base,
                new_title,
                mark_ready,
            })
        }
        BranchPrState::Merged => Ok(PrDecision::Create { draft: !ready }),
    }
}

fn dry_run_entry(item: &PlannedBookmark, decision: &PrDecision) -> PushedBookmark {
    match decision {
        PrDecision::Create { draft } => PushedBookmark {
            bookmark: item.bookmark.clone(),
            base: item.base.clone(),
            pr_number: None,
            url: None,
            head_sha: item.commit_id.clone(),
            title: item.title.clone(),
            draft: *draft,
            action: SyncAction::Created,
        },
        PrDecision::Retarget {
            pr,
            new_title,
            mark_ready,
            ..
        } => PushedBookmark {
            bookmark: item.bookmark.clone(),
            base: item.base.clone(),
            pr_number: Some(pr.number),
            url: Some(pr.url.clone()),
            head_sha: item.commit_id.clone(),
            title: new_title.clone().unwrap_or_else(|| pr.title.clone()),
            draft: if *mark_ready { false } else { pr.draft },
            action: SyncAction::Retargeted,
        },
        PrDecision::Reopen {
            pr,
            new_title,
            mark_ready,
            ..
        } => PushedBookmark {
            bookmark: item.bookmark.clone(),
            base: item.base.clone(),
            pr_number: Some(pr.number),
            url: Some(pr.url.clone()),
            head_sha: item.commit_id.clone(),
            title: new_title.clone().unwrap_or_else(|| pr.title.clone()),
            draft: if *mark_ready { false } else { pr.draft },
            action: SyncAction::Reopened,
        },
        PrDecision::Unchanged { pr } => PushedBookmark {
            bookmark: item.bookmark.clone(),
            base: item.base.clone(),
            pr_number: Some(pr.number),
            url: Some(pr.url.clone()),
            head_sha: item.commit_id.clone(),
            title: pr.title.clone(),
            draft: pr.draft,
            action: SyncAction::Unchanged,
        },
    }
}

fn execute_decision(
    client: &Client,
    item: &PlannedBookmark,
    decision: &PrDecision,
    default_body: &str,
) -> Result<PushedBookmark> {
    match decision {
        PrDecision::Create { draft } => {
            let created = client.create_pr(
                &item.bookmark,
                &item.base,
                &item.title,
                default_body,
                *draft,
            )?;
            Ok(PushedBookmark {
                bookmark: item.bookmark.clone(),
                base: created.base_ref,
                pr_number: Some(created.number),
                url: Some(created.url),
                head_sha: item.commit_id.clone(),
                title: created.title,
                draft: created.draft,
                action: SyncAction::Created,
            })
        }
        PrDecision::Retarget {
            pr,
            new_base,
            new_title,
            mark_ready,
        } => {
            let updated = if new_base.is_some() || new_title.is_some() {
                client.update_pr(pr.number, new_base.as_deref(), new_title.as_deref(), None)?
            } else {
                pr.clone()
            };
            let draft = if *mark_ready {
                client.mark_pr_ready(&updated.node_id)?;
                false
            } else {
                updated.draft
            };
            Ok(PushedBookmark {
                bookmark: item.bookmark.clone(),
                base: updated.base_ref,
                pr_number: Some(updated.number),
                url: Some(updated.url),
                head_sha: item.commit_id.clone(),
                title: updated.title,
                draft,
                action: SyncAction::Retargeted,
            })
        }
        PrDecision::Reopen {
            pr,
            new_base,
            new_title,
            mark_ready,
        } => {
            let updated = client.update_pr(
                pr.number,
                new_base.as_deref(),
                new_title.as_deref(),
                Some("open"),
            )?;
            let draft = if *mark_ready {
                client.mark_pr_ready(&updated.node_id)?;
                false
            } else {
                updated.draft
            };
            Ok(PushedBookmark {
                bookmark: item.bookmark.clone(),
                base: updated.base_ref,
                pr_number: Some(updated.number),
                url: Some(updated.url),
                head_sha: item.commit_id.clone(),
                title: updated.title,
                draft,
                action: SyncAction::Reopened,
            })
        }
        PrDecision::Unchanged { pr } => Ok(PushedBookmark {
            bookmark: item.bookmark.clone(),
            base: pr.base_ref.clone(),
            pr_number: Some(pr.number),
            url: Some(pr.url.clone()),
            head_sha: item.commit_id.clone(),
            title: pr.title.clone(),
            draft: pr.draft,
            action: SyncAction::Unchanged,
        }),
    }
}

fn verify_remote_refs(
    decisions: &[(PlannedBookmark, PrDecision)],
    origin_bookmarks: &BTreeMap<String, RemoteBookmark>,
) -> Result<()> {
    for (item, _) in decisions {
        let remote = origin_bookmarks.get(&item.bookmark).with_context(|| {
            format!(
                "Bookmark `{}` was not found on origin after push",
                item.bookmark
            )
        })?;
        ensure!(
            !remote.conflict && remote.target.eq_ignore_ascii_case(&item.commit_id),
            "Remote bookmark `{}@origin` ({}) does not match local commit {}",
            item.bookmark,
            remote.target,
            item.commit_id
        );
        ensure!(
            origin_bookmarks.contains_key(&item.base),
            "Base bookmark `{}` for `{}` does not exist on origin",
            item.base,
            item.bookmark
        );
    }
    Ok(())
}

fn print_entry(entry: &PushedBookmark, dry_run: bool) {
    let status = if dry_run {
        entry.action.dry_run_label()
    } else {
        entry.action.label()
    };
    let draft_tag = if entry.draft { " [draft]" } else { "" };
    match (&entry.pr_number, &entry.url) {
        (Some(num), Some(url)) => {
            println!(
                "{status}: #{num}{draft_tag} ({} -> {}) {url}",
                entry.bookmark, entry.base
            );
        }
        _ => {
            println!(
                "{status}:{draft_tag} ({} -> {}) \"{}\"",
                entry.bookmark, entry.base, entry.title
            );
        }
    }
}

fn read_pr_template(root: &Path) -> Result<String> {
    let path = root.join(".github/pull_request_template.md");
    if path.try_exists().unwrap_or(false) {
        let content =
            fs::read_to_string(&path).with_context(|| format!("Cannot read {}", path.display()))?;
        return Ok(content.trim().to_owned());
    }
    Ok(String::new())
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn ensure_not_review_stack(root: &Path, ignore_wc: bool) -> Result<()> {
    let output = run_jj(
        root,
        ignore_wc,
        &[
            "log",
            "--no-graph",
            "-r",
            r#"@ & bookmarks(glob:"review/*")::"#,
            "-T",
            r#"commit_id ++ "\n""#,
        ],
        "check whether @ is on a review stack",
    )?;
    ensure!(
        output.trim().is_empty(),
        "Current revision @ is on a review/pr-* stack; run `zed-pr-review close` or switch to your own stack before pushing"
    );
    Ok(())
}

fn read_origin_bookmarks(root: &Path, ignore_wc: bool) -> Result<BTreeMap<String, RemoteBookmark>> {
    let output = run_jj(
        root,
        ignore_wc,
        &[
            "bookmark",
            "list",
            "--remote",
            "origin",
            "-T",
            ORIGIN_BOOKMARK_TEMPLATE,
        ],
        "list origin remote bookmarks",
    )?;
    let mut map = BTreeMap::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let Some(name) = parts.next() else {
            continue;
        };
        let target = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        let conflict = parts.next().unwrap_or("false").trim() == "true";
        map.insert(name.to_owned(), RemoteBookmark { target, conflict });
    }
    Ok(map)
}

fn resolve_trunk_bookmark(
    root: &Path,
    git_dir: &Path,
    origin_bookmarks: &BTreeMap<String, RemoteBookmark>,
) -> Result<String> {
    // 1. Check origin/HEAD symbolic ref if present and tracked on origin.
    if let Ok(output) = Command::new("git")
        .current_dir(root)
        .args(["--no-pager", "--git-dir"])
        .arg(git_dir)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output()
        && output.status.success()
    {
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if let Some(name) = raw.strip_prefix("origin/")
            && !name.is_empty()
            && origin_bookmarks.contains_key(name)
        {
            return Ok(name.to_owned());
        }
    }

    // 2. Ask jj for remote origin bookmarks on `trunk()`.
    let output = run_jj(
        root,
        true,
        &[
            "log",
            "--no-graph",
            "-r",
            "trunk()",
            "-T",
            r#"remote_bookmarks.filter(|b| b.remote() == "origin").map(|b| b.name() ++ "\n")"#,
        ],
        "resolve trunk() bookmark on origin",
    )?;
    let candidates: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|s| {
            !s.is_empty()
                && !s.starts_with(BOOKMARK_PREFIX)
                && !s.starts_with(INTERNAL_BOOKMARK_PREFIX)
        })
        .map(ToOwned::to_owned)
        .collect();

    for preferred in ["main", "master", "trunk"] {
        if candidates.iter().any(|c| c == preferred) {
            return Ok(preferred.to_owned());
        }
    }
    if candidates.len() == 1 {
        return Ok(candidates[0].clone());
    }

    // 3. Fall back to well-known default branch names present on origin.
    for preferred in ["main", "master", "trunk"] {
        if origin_bookmarks.contains_key(preferred) {
            return Ok(preferred.to_owned());
        }
    }

    bail!("Cannot determine trunk bookmark on origin (expected main, master, or origin/HEAD)")
}

fn read_commits(root: &Path, revset: &str, ignore_wc: bool) -> Result<Vec<CommitInfo>> {
    let output = run_jj(
        root,
        ignore_wc,
        &["log", "--no-graph", "-r", revset, "-T", COMMIT_TEMPLATE],
        &format!("inspect jj revset `{revset}`"),
    )?;
    let mut commits = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let raw: RawCommitInfo = serde_json::from_str(line)
            .with_context(|| format!("Invalid jj commit JSON: {line}"))?;
        ensure!(
            raw.local_bookmark_names.len() == raw.local_bookmark_conflicts.len(),
            "Mismatched bookmark metadata arrays from jj"
        );
        let local_bookmarks = raw
            .local_bookmark_names
            .into_iter()
            .zip(raw.local_bookmark_conflicts)
            .map(|(name, conflict)| BookmarkRef { name, conflict })
            .collect();
        commits.push(CommitInfo {
            commit_id: raw.commit_id,
            change_id: raw.change_id,
            parents: raw.parents,
            description: raw.description,
            conflict: raw.conflict,
            immutable: raw.immutable,
            local_bookmarks,
        });
    }
    Ok(commits)
}

fn push_bookmark(root: &Path, bookmark: &str, allow_empty_description: bool) -> Result<()> {
    let mut args = vec!["git", "push", "--remote", "origin", "--bookmark", bookmark];
    if allow_empty_description {
        args.push("--allow-empty-description");
    }
    run_jj(
        root,
        false,
        &args,
        &format!("push bookmark `{bookmark}` to origin"),
    )?;
    Ok(())
}

fn run_jj(root: &Path, ignore_wc: bool, args: &[&str], context: &str) -> Result<String> {
    let mut cmd = Command::new("jj");
    cmd.current_dir(root)
        .env("JJ_PAGER", "cat")
        .args(["--no-pager", "--color=never"]);
    if ignore_wc {
        cmd.arg("--ignore-working-copy");
    }
    cmd.args(args);
    let output = cmd
        .output()
        .with_context(|| format!("Cannot start jj to {context}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("Failed to {context}: {stderr}");
    }
    String::from_utf8(output.stdout).context("jj returned non-UTF-8 output")
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";

    let status = Command::new(program)
        .arg(url)
        .status()
        .with_context(|| format!("Cannot launch `{program}` to open {url}"))?;
    ensure!(status.success(), "`{program} {url}` failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, parents: &[&str], description: &str, bookmarks: &[&str]) -> CommitInfo {
        CommitInfo {
            commit_id: id.to_owned(),
            change_id: format!("chg_{id}"),
            parents: parents.iter().map(|s| (*s).to_owned()).collect(),
            description: description.to_owned(),
            conflict: false,
            immutable: false,
            local_bookmarks: bookmarks
                .iter()
                .map(|b| BookmarkRef {
                    name: (*b).to_owned(),
                    conflict: false,
                })
                .collect(),
        }
    }

    fn remote_map(names: &[&str]) -> BTreeMap<String, RemoteBookmark> {
        names
            .iter()
            .map(|n| {
                (
                    (*n).to_owned(),
                    RemoteBookmark {
                        target: "0".repeat(40),
                        conflict: false,
                    },
                )
            })
            .collect()
    }

    fn branch_pr(
        number: u64,
        head: &str,
        base: &str,
        state: BranchPrState,
        draft: bool,
        author: &str,
    ) -> BranchPr {
        BranchPr {
            number,
            node_id: format!("PR_{number}"),
            state,
            draft,
            title: format!("Title {number}"),
            head_ref: head.to_owned(),
            head_sha: "a".repeat(40),
            base_ref: base.to_owned(),
            url: format!("https://github.com/owner/repo/pull/{number}"),
            author: author.to_owned(),
        }
    }

    #[test]
    fn default_scope_unpushed_pushes_only_up_to_at() {
        let c1 = commit("c1", &["trunk"], "first", &["stefan/1"]);
        let c2 = commit("c2", &["c1"], "second", &["stefan/2"]);
        let c3 = commit("c3", &["c2"], "third wip", &["stefan/3"]);
        let commits: HashMap<_, _> = [c1, c2, c3]
            .into_iter()
            .map(|c| (c.commit_id.clone(), c))
            .collect();
        let up_to_at: HashSet<_> = ["c1", "c2"].into_iter().map(String::from).collect();
        let descendants: HashSet<_> = ["c3"].into_iter().map(String::from).collect();
        let origin = remote_map(&["main"]);

        let selected = select_default_commits(&commits, &up_to_at, &descendants, &origin, "main");
        assert_eq!(
            selected,
            ["c1", "c2"].into_iter().map(String::from).collect()
        );
    }

    #[test]
    fn default_scope_includes_pushed_descendants_and_unpushed_bridge() {
        // @ is at c1; c2 is unpushed; c3 is already on origin; c4 is unpushed WIP above c3.
        let c1 = commit("c1", &["trunk"], "first", &["stefan/1"]);
        let c2 = commit("c2", &["c1"], "second bridge", &["stefan/2"]);
        let c3 = commit("c3", &["c2"], "third pushed", &["stefan/3"]);
        let c4 = commit("c4", &["c3"], "fourth wip", &["stefan/4"]);
        let commits: HashMap<_, _> = [c1, c2, c3, c4]
            .into_iter()
            .map(|c| (c.commit_id.clone(), c))
            .collect();
        let up_to_at: HashSet<_> = ["c1"].into_iter().map(String::from).collect();
        let descendants: HashSet<_> = ["c2", "c3", "c4"].into_iter().map(String::from).collect();
        let origin = remote_map(&["main", "stefan/1", "stefan/3"]);

        let selected = select_default_commits(&commits, &up_to_at, &descendants, &origin, "main");
        assert_eq!(
            selected,
            ["c1", "c2", "c3"].into_iter().map(String::from).collect()
        );
    }

    #[test]
    fn plan_orders_bottom_to_top_and_resolves_bases_across_unbookmarked_commits() {
        // trunk_old (not in graph because in ::trunk()) -> c1 (bm1) -> mid (no bm) -> c2 (bm2)
        let c1 = commit("c1", &["trunk_old"], "first feature", &["stefan/1"]);
        let mid = commit("mid", &["c1"], "helper commit", &[]);
        let c2 = commit("c2", &["mid"], "second feature", &["stefan/2"]);
        let commits: HashMap<_, _> = [c1, mid, c2]
            .into_iter()
            .map(|c| (c.commit_id.clone(), c))
            .collect();
        let selected: HashSet<_> = ["c1", "c2"].into_iter().map(String::from).collect();
        let origin = remote_map(&["main"]);

        let plan = plan_from_graph(&commits, &selected, &origin, "main", false).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].bookmark, "stefan/1");
        assert_eq!(plan[0].base, "main");
        assert_eq!(plan[1].bookmark, "stefan/2");
        assert_eq!(plan[1].base, "stefan/1");
    }

    #[test]
    fn excludes_review_bookmarks_and_rejects_ambiguous_multi_bookmark() {
        let mut c1 = commit("c1", &["trunk"], "first", &["stefan/1", "review/pr-99"]);
        let commits: HashMap<_, _> = [(c1.commit_id.clone(), c1.clone())].into_iter().collect();
        let selected: HashSet<_> = ["c1"].into_iter().map(String::from).collect();
        let origin = remote_map(&["main"]);
        let plan = plan_from_graph(&commits, &selected, &origin, "main", false).unwrap();
        assert_eq!(plan[0].bookmark, "stefan/1");

        // Now add a second non-review local bookmark -> must error.
        c1.local_bookmarks.push(BookmarkRef {
            name: "stefan/alt".to_owned(),
            conflict: false,
        });
        let commits: HashMap<_, _> = [(c1.commit_id.clone(), c1)].into_iter().collect();
        let err = plan_from_graph(&commits, &selected, &origin, "main", false).unwrap_err();
        assert!(err.to_string().contains("multiple local bookmarks"));
    }

    #[test]
    fn title_truncation_respects_utf8_char_boundaries_and_fallback() {
        let long = "é".repeat(80);
        let title = format_pr_title(&format!("{long}\nsecond line"), "stefan/1");
        assert_eq!(title.chars().count(), 70);
        assert_eq!(format_pr_title("   \n  ", "stefan/1"), "stefan/1");
    }

    #[test]
    fn pr_decision_skips_noop_patch_and_enforces_ownership() {
        let item = PlannedBookmark {
            bookmark: "stefan/1".to_owned(),
            base: "main".to_owned(),
            commit_id: "a".repeat(40),
            title: "Title 10".to_owned(),
        };
        let pr = branch_pr(10, "stefan/1", "main", BranchPrState::Open, true, "stefan");

        let decision =
            decide_pr_action(&item, &[pr.clone()], "stefan", false, false, false).unwrap();
        assert!(matches!(decision, PrDecision::Unchanged { .. }));

        // Foreign owner must be rejected.
        let foreign = branch_pr(11, "stefan/1", "main", BranchPrState::Open, true, "alice");
        let err = decide_pr_action(&item, &[foreign], "stefan", false, false, false).unwrap_err();
        assert!(err.to_string().contains("owned by @alice"));
    }
}
