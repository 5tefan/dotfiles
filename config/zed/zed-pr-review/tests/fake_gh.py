#!/usr/bin/env python3
"""Strict, offline gh fixture. Every invocation is recorded before responding.

Session controls:
- files overrides the GET files collection (one page), including patches.
- head_repo defaults to acme/widgets; set it to model a fork PR.
- additional_pr_numbers allows explicit-mismatch tests to resolve another PR
  with the same commits. Other tests still accept only PR 17.
- prs maps explicit PR numbers (as strings) to overrides of the flat PR controls:
  head, base, head_ref, base_ref, head_repo, title, author, draft, files, etc.
  These PRs have distinct URLs and node IDs; unspecified fields keep the defaults.
- pulls_pages enables the exact paginated open-pulls endpoint. Each page contains
  registered PR numbers, or raw REST objects for malformed/stale listing tests.

Ready controls:
- host defaults to github.com and must match every explicit API hostname.
- draft defaults to True; omit_draft removes the response field entirely.
- node_id defaults to PR_fixture17; omit_node_id removes the response field.
- base_ref defaults to main, independently of the base SHA.
- ready / request_reviewer: success (default), fail, or lost_response.
- repo_view_changes / pr_list_changes: patches before responding to discovery.
- pr_read_changes: patches keyed by the 1-based GET PR count, before responding.
- ready_after / request_reviewer_after: patches after an accepted mutation,
  including lost_response. A fail does not apply the mutation or its patch.
Patches update control fields; jj_new=True also moves the fixture checkout to a
new child commit and records its SHA as jj_new_head so tests can check it is left
untouched. pr_reads and requested_reviewers persist across invocations.
requested_reviewers contains logins or org/team strings; REST requests add to it
without removing existing entries. Ready writes use only explicit-host gh api.
"""
import json
import os
import re
import subprocess
import sys
from pathlib import Path

control_path = Path(os.environ["ZPR_TEST_CONTROL"])
control = json.loads(control_path.read_text())
log_path = Path(os.environ["ZPR_TEST_GH_LOG"])
args = sys.argv[1:]
record = {"args": args, "cwd": os.getcwd(), "git_dir": os.environ.get("GIT_DIR")}
repo = "acme/widgets"
host = control.get("host", "github.com")
reviewer = control.get("reviewer", "reviewer")
number = 17
url = f"https://{host}/{repo}/pull/{number}"
endpoint_root = f"repos/{repo}/pulls/{number}"


def log():
    with log_path.open("a") as stream:
        stream.write(json.dumps(record) + "\n")


def unexpected(message):
    record["unexpected"] = message
    log()
    print(message, file=sys.stderr)
    sys.exit(97)


def emit(value):
    log()
    print(json.dumps(value))
    sys.exit(0)


def save_control():
    temporary = control_path.with_suffix(".next")
    temporary.write_text(json.dumps(control))
    temporary.replace(control_path)


def discovery_allowed():
    if control.get("deny_discovery"):
        record["discovery_denied"] = True
        log()
        sys.exit(3)


def apply_changes(changes):
    changes = dict(changes)
    if changes.pop("jj_new", False):
        result = subprocess.run(
            ["jj", "--no-pager", "new", "-m", "Checkout moved by fake gh"],
            capture_output=True, text=True, timeout=10,
        )
        if result.returncode:
            unexpected("Cannot move fixture checkout: " + result.stderr)
        head = subprocess.run(
            ["jj", "--no-pager", "--ignore-working-copy", "log", "-r", "@",
             "--no-graph", "-T", "commit_id"],
            capture_output=True, text=True, timeout=10,
        )
        if head.returncode:
            unexpected("Cannot read moved fixture checkout: " + head.stderr)
        control["jj_new_head"] = head.stdout.strip()
    control.update(changes)


def mutate_ready(kind, mode_key, response, requested=None):
    mode = control.get(mode_key, "success")
    if mode not in ("success", "fail", "lost_response"):
        unexpected("Unsupported mutation mode: " + str(mode))
    record["mutation"] = kind
    if requested is not None:
        record["reviewer"] = requested
    log()
    if mode == "fail":
        print(f"simulated {kind} failure before acceptance", file=sys.stderr)
        sys.exit(1)
    if kind == "ready":
        control["draft"] = False
    else:
        reviewers = control.setdefault("requested_reviewers", [])
        if requested not in reviewers:
            reviewers.append(requested)
    apply_changes(control.get(mode_key + "_after", {}))
    save_control()
    if mode == "lost_response":
        print(f"connection lost after the server accepted {kind}", file=sys.stderr)
        sys.exit(1)
    print(json.dumps(response))
    sys.exit(0)


def pr_control(candidate):
    values = dict(control)
    values.update(control.get("prs", {}).get(str(candidate), {}))
    return values


def pr_response(candidate):
    values = pr_control(candidate)
    response = {
        "number": candidate,
        "html_url": f"https://{host}/{repo}/pull/{candidate}",
        "title": values.get("title", "Offline review fixture"),
        "state": values.get("pr_state", "open"), "merged": False, "merged_at": None,
        "user": {"login": values.get("author", "author")},
        "head": {"sha": values["head"], "ref": values.get("head_ref", "topic"),
                 "repo": {"full_name": values.get("head_repo", repo)}},
        "base": {"sha": values["base"], "ref": values.get("base_ref", "main"),
                 "repo": {"full_name": repo}},
    }
    if not values.get("omit_draft"):
        response["draft"] = values.get("draft", True)
    if not values.get("omit_node_id"):
        response["node_id"] = values.get("node_id", f"PR_fixture{candidate}")
    return response


def graphql_tokens(query):
    # Ignore GraphQL whitespace and commas, but keep field names separate.
    return re.findall(r"[_A-Za-z][_0-9A-Za-z]*|[^\s,]", query)

if args == ["repo", "view", "--json", "nameWithOwner,url"]:
    discovery_allowed()
    if "repo_view_changes" in control:
        apply_changes(control["repo_view_changes"])
        save_control()
    emit({"nameWithOwner": repo, "url": f"https://{host}/{repo}"})

if args[:2] == ["pr", "list"]:
    expected = [
        "pr", "list", "--repo", f"{host}/{repo}", "--head", "topic",
        "--state", "open", "--limit", "100", "--json",
        "number,headRefOid,url,headRepository,headRepositoryOwner",
    ]
    if args != expected:
        unexpected("Unexpected gh pr list arguments")
    discovery_allowed()
    if "pr_list_changes" in control:
        apply_changes(control["pr_list_changes"])
        save_control()
    if control.get("pr_state", "open") != "open":
        emit([])
    emit([{
        "number": number, "headRefOid": control["head"], "url": url,
        "headRepository": {"name": "widgets", "nameWithOwner": repo},
        "headRepositoryOwner": {"login": "acme"},
    }])

if not args or args[0] != "api":
    unexpected("Unsupported gh command")

options = {}
headers = []
flags = set()
endpoint = None
index = 1
while index < len(args):
    item = args[index]
    if item in ("--hostname", "--method", "--header", "--input"):
        if index + 1 == len(args):
            unexpected("Missing gh api option value")
        value = args[index + 1]
        if item == "--header":
            headers.append(value)
        elif item in options:
            unexpected("Duplicate gh api option")
        else:
            options[item] = value
        index += 2
    elif item in ("--paginate", "--slurp"):
        flags.add(item)
        index += 1
    elif not item.startswith("-") and endpoint is None:
        endpoint = item
        index += 1
    else:
        unexpected("Unsupported gh api argument: " + item)

if options.get("--hostname") != host:
    unexpected("API host must be explicit and match the discovered host")
if set(headers) != {
    "Accept: application/vnd.github+json", "X-GitHub-Api-Version: 2022-11-28"
}:
    unexpected("Missing REST API headers")
method = options.get("--method")
record.update(method=method, endpoint=endpoint, hostname=options["--hostname"])

registered_numbers = {17, *control.get("additional_pr_numbers", [])}
registered_numbers.update(int(candidate) for candidate in control.get("prs", {}))
for candidate in sorted(registered_numbers):
    candidate_root = f"repos/{repo}/pulls/{candidate}"
    if endpoint == candidate_root or (endpoint and endpoint.startswith(candidate_root + "/")):
        number = candidate
        endpoint_root = candidate_root
        url = f"https://{host}/{repo}/pull/{number}"
        break

if method == "POST":
    if options.get("--input") != "-" or flags:
        unexpected("POST requires --input - and no pagination")
    payload = json.load(sys.stdin)
    record["payload"] = payload
    if endpoint == "graphql":
        query = """mutation($id: ID!) {
            markPullRequestReadyForReview(input: {pullRequestId: $id}) {
                pullRequest { id isDraft }
            }
        }"""
        node_id = control.get("node_id", "PR_fixture17")
        if (
            not isinstance(payload, dict)
            or set(payload) != {"query", "variables"}
            or not isinstance(payload["query"], str)
            or graphql_tokens(payload["query"]) != graphql_tokens(query)
            or payload["variables"] != {"id": node_id}
        ):
            unexpected("Expected only markPullRequestReadyForReview for the REST node_id")
        response = {"data": {"markPullRequestReadyForReview": {
            "pullRequest": {"id": node_id, "isDraft": False},
        }}}
        mutate_ready("ready", "ready", response)
    if endpoint == endpoint_root + "/requested_reviewers":
        if not isinstance(payload, dict) or set(payload) not in (
            {"reviewers"}, {"team_reviewers"},
        ):
            unexpected("Expected reviewers OR team_reviewers, without extra fields")
        field = next(iter(payload))
        values = payload[field]
        if (
            not isinstance(values, list) or len(values) != 1
            or not isinstance(values[0], str)
            or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*", values[0])
        ):
            unexpected("Expected one reviewer login or unqualified team slug")
        requested = values[0]
        if field == "team_reviewers":
            requested = repo.split("/", 1)[0] + "/" + requested
        mutate_ready("request-reviewer", "request_reviewer", {}, requested)
    if endpoint != endpoint_root + "/reviews":
        unexpected("Unsupported POST endpoint")
    drafts = list(Path(os.environ["ZPR_TEST_STATE"]).glob("*/draft.json"))
    record["drafts_at_post"] = [json.loads(path.read_text()) for path in drafts]
    if "observe_sessions" in control:
        record["sessions_at_post"] = {
            directory: {name: (Path(directory) / name).exists()
                        for name in ("head", "base", "cleanup.json")}
            for directory in control["observe_sessions"]
        }
    log()
    mode = control.get("post", "success")
    if mode == "fail":
        print("simulated connection failure", file=sys.stderr)
        sys.exit(1)
    review_id = 501
    review = {
        "id": review_id, "html_url": url + f"#pullrequestreview-{review_id}",
        "state": {"COMMENT": "COMMENTED", "APPROVE": "APPROVED", "REQUEST_CHANGES": "CHANGES_REQUESTED"}[payload["event"]],
        "body": payload["body"], "commit_id": payload["commit_id"],
        "user": {"login": reviewer}, "submitted_at": "2026-01-01T00:00:00Z",
    }
    comments = []
    for index, comment in enumerate(payload.get("comments", [])):
        comments.append({
            **comment, "id": 1000 + index, "pull_request_review_id": review_id,
            "user": {"login": reviewer}, "commit_id": payload["commit_id"],
            "original_commit_id": payload["commit_id"],
            "original_line": comment["line"],
            "original_start_line": comment.get("start_line"),
            "start_line": comment.get("start_line"),
            "start_side": comment.get("start_side"),
        })
    if mode == "delayed_acceptance":
        control["delayed_review"] = review
        control["delayed_comments"] = comments
    else:
        control.setdefault("reviews", []).append(review)
        control.setdefault("review_comments", {})[str(review_id)] = comments
    save_control()
    if mode in ("lost_response", "delayed_acceptance"):
        print("connection lost after the server accepted the review", file=sys.stderr)
        sys.exit(1)
    if mode == "receipt_save_failure":
        # Preserve the on-disk attempt, but make Store::save reject the receipt.
        # A hard link is deterministic even when tests run with elevated rights.
        matching = [path for path in drafts
                    if json.loads(path.read_text())["pr"]["number"] == number]
        assert len(matching) == 1, matching
        os.link(matching[0], matching[0].with_name("receipt-save-blocker"))
    if mode == "invalid_receipt":
        review = dict(review, body="Response does not match the submitted review")
    print(json.dumps(review))
    sys.exit(0)

if method != "GET" or "--input" in options:
    unexpected("Expected an explicit GET without a request body")

if endpoint == "user":
    if flags:
        unexpected("User endpoint is not paginated")
    emit({"login": reviewer})

if endpoint == endpoint_root:
    discovery_allowed()
    if flags:
        unexpected("PR endpoint is not paginated")
    count = control.get("pr_reads", 0) + 1
    control["pr_reads"] = count
    record["pr_read"] = count
    apply_changes(control.get("pr_read_changes", {}).get(str(count), {}))
    save_control()
    emit(pr_response(number))

if flags != {"--paginate", "--slurp"}:
    unexpected("Collection requests must use --paginate --slurp")

if endpoint == f"repos/{repo}/pulls?state=open&per_page=100":
    discovery_allowed()
    if "pulls_pages" not in control:
        unexpected("Open-pulls listing requires explicit pulls_pages control")
    pages = []
    for page in control["pulls_pages"]:
        values = []
        for candidate in page:
            if isinstance(candidate, dict):
                values.append(candidate)
            elif type(candidate) is int and candidate in registered_numbers:
                values.append(pr_response(candidate))
            else:
                unexpected("Unregistered PR in pulls_pages: " + str(candidate))
        pages.append(values)
    emit(pages)

if endpoint == endpoint_root + "/files?per_page=100":
    emit([pr_control(number).get("files", [{
        "filename": "review file.txt", "status": "added",
        "patch": "@@ -0,0 +1,3 @@\n+one\n+two\n+three",
    }])])
if endpoint == endpoint_root + "/reviews?per_page=100":
    emit([control.get("reviews", [])])
prefix = endpoint_root + "/reviews/"
if endpoint and endpoint.startswith(prefix) and endpoint.endswith("/comments?per_page=100"):
    review_id = endpoint[len(prefix):].split("/", 1)[0]
    comments = control.get("review_comments", {}).get(review_id)
    if comments is None:
        unexpected("No fixture comments for review " + review_id)
    emit([comments])
unexpected("Unsupported API endpoint")
