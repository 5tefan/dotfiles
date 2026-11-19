#!/bin/sh
set -eu

export JJ_PAGER=cat

# Include ancestors so this also works from an unbookmarked change above a PR.
bookmark=$(jj --ignore-working-copy --color never bookmark list \
    -r 'heads(::@ & bookmarks())' \
    -T 'if(!remote, if(conflict, "conflict:") ++ name ++ "\n")')

case "$bookmark" in
    '')
        printf '%s\n' 'No bookmark found on the current change or its ancestors.' >&2
        exit 1
        ;;
    conflict:* | *'
'*)
        printf 'Expected one unambiguous bookmark; found:\n%s\n' "$bookmark" >&2
        exit 1
        ;;
esac

# Let gh find the repository even when the jj workspace is not colocated.
GIT_DIR=$(jj --ignore-working-copy git root)
export GIT_DIR

printf 'Looking up the open PR for %s...\n' "$bookmark"
pr=$(gh pr list --head "$bookmark" --state open --limit 2 \
    --json url,isDraft --jq '
        if length == 0 then error("No open PR found for this bookmark")
        elif length > 1 then error("Multiple open PRs found for this bookmark")
        else .[0] | "\(.url) \(.isDraft)"
        end')
read -r url draft <<EOF
$pr
EOF

if [ "$draft" = true ]; then
    gh pr ready "$url"
fi

reviewer=${ZED_REVISION_READY_REVIEWER:-}
case "$reviewer" in
    *[![:space:]]*)
        gh pr edit "$url" --add-reviewer "$reviewer"
        printf 'Ready for review; requested %s: %s\n' "$reviewer" "$url"
        ;;
    *)
        printf 'Ready for review: %s\n' "$url"
        ;;
esac
