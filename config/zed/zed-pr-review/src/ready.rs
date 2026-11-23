use crate::github::{Client, ReadyState, reviewer_payload, validate_requested_reviewer};
use crate::model::PullRequest;
use anyhow::{Context, Result, ensure};

pub fn requested_reviewer(override_reviewer: Option<&str>) -> Result<Option<String>> {
    let reviewer = if let Some(value) = override_reviewer {
        ensure!(!value.trim().is_empty(), "--reviewer must not be blank");
        value.to_owned()
    } else {
        match std::env::var("ZED_REVISION_READY_REVIEWER") {
            Ok(value) if !value.trim().is_empty() => value,
            Ok(_) | Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(error) => return Err(error).context("ZED_REVISION_READY_REVIEWER must be UTF-8"),
        }
    };
    let reviewer = reviewer.trim();
    validate_requested_reviewer(reviewer)?;
    Ok(Some(reviewer.to_owned()))
}

/// Operate on the PR discovered for the initial jj `@`, not the live checkout.
/// Readiness is independent of private review drafts, including uncertain submissions.
pub fn run(client: &Client, pr: &PullRequest, reviewer: Option<&str>) -> Result<()> {
    if let Some(reviewer) = reviewer {
        reviewer_payload(&pr.repo, reviewer)?;
    }
    let pinned = client.read_ready_state(pr.number)?;
    ensure!(
        pr.same_revision(&pinned.pr),
        "The PR head, base, or identity changed since discovery"
    );
    client.check_identity(pr)?;
    println!("PR head at captured jj @: {}", pr.head_sha);
    if checked_state(client, &pinned)? {
        let outcome = client.mark_ready(&pinned);
        // Recheck even after a failed command: the server may have accepted it.
        let verification = verify_ready(client, &pinned);
        if let Err(error) = outcome {
            if let Err(check) = verification {
                eprintln!("After marking ready: {check:#}");
            }
            return Err(error).context("Marking ready failed or its outcome is uncertain. No reviewer was requested. Check GitHub; no automatic retry was attempted");
        }
        verification.context("GitHub accepted the mark-ready command, but verification failed. No reviewer was requested. Check GitHub; nothing was rolled back")?;
        println!("Marked ready for review: {}", pr.url);
    } else {
        println!("Already ready for review: {}", pr.url);
    }

    if let Some(reviewer) = reviewer {
        verify_ready(client, &pinned)
            .context("PR changed before requesting the reviewer; no reviewer was requested")?;
        let outcome = client.request_reviewer(pr, reviewer);
        let verification = verify_ready(client, &pinned);
        if let Err(error) = outcome {
            if let Err(check) = verification {
                eprintln!("After requesting reviewer: {check:#}");
            }
            return Err(error).context("The PR was ready for review, but requesting the reviewer failed or its outcome is uncertain. Check GitHub before retrying; no automatic retry was attempted");
        }
        verification.context("GitHub accepted the reviewer request, but verification failed. Check GitHub; the request was not rolled back")?;
        println!("Requested {reviewer}: {}", pr.url);
    }
    Ok(())
}

fn checked_state(client: &Client, pinned: &ReadyState) -> Result<bool> {
    let current = client.read_ready_state(pinned.pr.number)?;
    ensure!(
        pinned.pr.same_revision(&current.pr)
            && pinned.base_ref == current.base_ref
            && pinned.node_id == current.node_id,
        "The PR head, base branch, or identity changed. Check out the exact PR head and retry"
    );
    client.check_identity(&pinned.pr)?;
    Ok(current.draft)
}

fn verify_ready(client: &Client, pinned: &ReadyState) -> Result<()> {
    ensure!(
        !checked_state(client, pinned)?,
        "The PR is still a draft or was changed back to a draft"
    );
    Ok(())
}
