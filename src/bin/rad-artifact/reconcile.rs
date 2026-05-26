//! `rad-artifact reconcile` — fix COB drift relative to what the local
//! node is actively seeding.
//!
//! Three drift classes (per the design doc):
//! - **MissingLocation**: the node is seeding a `(rid, cid)` pair but
//!   no `iroh://{current_endpoint}` location for that CID exists in
//!   any release under our DID. Auto-fixed by writing the location to
//!   the most recent matching release.
//! - **OrphanedSelf**: a `iroh://{current_endpoint}` location under our
//!   DID exists for a CID we are *not* seeding. Flagged but only
//!   retracted when the user passes `--retract-orphaned <CID>` for
//!   that specific CID.
//! - **StaleEndpoint**: an `iroh://{other_endpoint}` location under our
//!   DID — i.e. one of our previous keys, or a malformed/legacy-encoded
//!   endpoint id that no longer decodes (e.g. a hex host left over from
//!   a previous encoding). Reported in summary; retracted only when
//!   `--retract-orphaned-self` is passed.

use std::collections::HashSet;
use std::str::FromStr;
use std::time::Duration;

use clap::Parser;
use radicle::{
    identity::{Did, RepoId},
    prelude::{Profile, ReadStorage},
    storage::git::Repository,
};
use radicle_artifact::client::{self, Client};
use radicle_artifact::protocol::{Command as NodeMsg, SeededEntry, Status};
use radicle_artifact::seeder::keys::encode_endpoint_id;
use radicle_artifact::share::iroh_url;
use radicle_artifact::Cid;
use thiserror::Error;
use url::Url;

use crate::node;
use crate::{open_releases, open_repo};

/// `rad-artifact reconcile` clap entry.
#[derive(Parser)]
pub struct Cli {
    /// Reconcile every repository in local storage instead of just
    /// the current one.
    #[clap(long)]
    pub all_repos: bool,
    /// Retract our `iroh://{current_endpoint}` location for this CID,
    /// which the node is no longer seeding. Repeatable.
    #[clap(long = "retract-orphaned", value_name = "CID")]
    pub retract_orphaned: Vec<Cid>,
    /// Retract `iroh://{other_endpoint}` locations under our DID
    /// (URLs whose endpoint id does not match the running node).
    #[clap(long)]
    pub retract_orphaned_self: bool,
}

/// Reconcile failures.
#[derive(Debug, Error)]
pub enum Error {
    /// Most failures route through the node module's error.
    #[error(transparent)]
    Node(#[from] node::Error),
    /// Repository listing failed.
    #[error("failed to list local repositories: {0}")]
    Storage(String),
    /// Could not open a repository in storage.
    #[error("failed to open repository {rid}: {err}")]
    Repository {
        /// Repository that we tried to open.
        rid: RepoId,
        /// Underlying storage error.
        err: String,
    },
}

/// One per-repo run.
#[derive(Default, Debug)]
struct RepoReport {
    added: u32,
    retracted: u32,
    orphaned_self_skipped: Vec<Cid>,
    stale_endpoint_skipped: u32,
}

/// Carries everything `reconcile_one` needs to inspect a single repo
/// and apply the chosen retraction policy.
struct ReconcileCtx<'a> {
    client: &'a Client,
    endpoint_id: &'a str,
    local_did: &'a Did,
    retract_orphaned: &'a HashSet<Cid>,
    retract_orphaned_self: bool,
}

/// Entry point for `rad-artifact reconcile`.
pub fn run(cli: Cli, repo_override: Option<RepoId>, profile: &Profile) -> Result<(), Error> {
    // We need the node up to know our current endpoint id and to ask
    // about seeded tags.
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    let status = client
        .call_blocking::<Status>(&NodeMsg::Status, client::DEFAULT_TIMEOUT)
        .map_err(|e| Error::Node(node::client_err(e)))?;

    let endpoint_id = status.endpoint_id;
    let local_did = Did::from(*profile.id());

    let target_rids: Vec<RepoId> = if cli.all_repos {
        profile
            .storage
            .repositories()
            .map_err(|e| Error::Storage(e.to_string()))?
            .into_iter()
            .map(|info| info.rid)
            .collect()
    } else {
        let repo = open_repo(repo_override, profile).map_err(|e| Error::Storage(e.to_string()))?;
        vec![repo.id]
    };

    let retract_orphaned: HashSet<Cid> = cli.retract_orphaned.iter().copied().collect();
    let ctx = ReconcileCtx {
        client: &client,
        endpoint_id: &endpoint_id,
        local_did: &local_did,
        retract_orphaned: &retract_orphaned,
        retract_orphaned_self: cli.retract_orphaned_self,
    };
    let mut total = RepoReport::default();

    for rid in target_rids {
        let repo = profile
            .storage
            .repository(rid)
            .map_err(|e| Error::Repository {
                rid,
                err: e.to_string(),
            })?;
        let report = reconcile_one(rid, &repo, &ctx, profile)?;
        total.added += report.added;
        total.retracted += report.retracted;
        total
            .orphaned_self_skipped
            .extend(report.orphaned_self_skipped);
        total.stale_endpoint_skipped += report.stale_endpoint_skipped;
    }

    print_summary(&total);
    Ok(())
}

fn reconcile_one(
    rid: RepoId,
    repo: &Repository,
    ctx: &ReconcileCtx<'_>,
    profile: &Profile,
) -> Result<RepoReport, Error> {
    let mut releases =
        open_releases(repo).map_err(|e| Error::Node(node::Error::Usage(e.to_string())))?;

    // Ask the node what's tagged for this rid.
    let entries: Vec<SeededEntry> = ctx
        .client
        .call_blocking(
            &NodeMsg::ListSeeded {
                rid: rid.to_string(),
            },
            Duration::from_secs(30),
        )
        .map_err(|e| Error::Node(node::client_err(e)))?;
    let seeded: HashSet<Cid> = entries
        .into_iter()
        .filter_map(|e| Cid::from_str(&e.cid).ok())
        .collect();

    // Snapshot every release once so the loop body can drop its borrow
    // before mutations begin.
    let all_releases: Vec<(radicle_artifact::ReleaseId, radicle_artifact::Release)> = releases
        .all()
        .map_err(|e| Error::Node(node::Error::Find(e)))?
        .filter_map(|r| r.ok())
        .map(|(oid, r)| (radicle_artifact::ReleaseId::from(oid), r))
        .collect();

    // Classify every iroh:// URL under our DID across every release.
    let mut current_endpoint_have: HashSet<Cid> = HashSet::new();
    let mut orphaned_self: Vec<(radicle_artifact::ReleaseId, Cid, Url)> = Vec::new();
    let mut stale_endpoint: Vec<(radicle_artifact::ReleaseId, Cid, Url)> = Vec::new();

    for (release_id, release) in &all_releases {
        for (cid, artifact) in release.artifacts() {
            let Some(urls) = artifact.locations_of(ctx.local_did) else {
                continue;
            };
            for url in urls.iter().filter(|u| iroh_url::matches(u)) {
                // Bare `iroh://` under our DID resolves to our current
                // endpoint id (key derived from same Ed25519 secret), so
                // treat it as current. A host that fails to decode is
                // treated as stale: it isn't our current endpoint id and
                // belongs in the same retraction bucket as a foreign one.
                let eid = match iroh_url::endpoint_id(url) {
                    Ok(Some(eid)) => encode_endpoint_id(&eid),
                    Ok(None) => ctx.endpoint_id.to_string(),
                    Err(_) => {
                        stale_endpoint.push((*release_id, *cid, url.clone()));
                        continue;
                    }
                };
                if eid == ctx.endpoint_id {
                    current_endpoint_have.insert(*cid);
                    if !seeded.contains(cid) {
                        orphaned_self.push((*release_id, *cid, url.clone()));
                    }
                } else {
                    stale_endpoint.push((*release_id, *cid, url.clone()));
                }
            }
        }
    }

    // Seeded CIDs missing a current-endpoint location: pick the most
    // recent matching release and queue an add.
    let mut missing: Vec<(radicle_artifact::ReleaseId, Cid)> = Vec::new();
    for cid in &seeded {
        if current_endpoint_have.contains(cid) {
            continue;
        }
        let target = all_releases
            .iter()
            .filter(|(_, r)| r.artifact(cid).is_some())
            .max_by_key(|(_, r)| r.timestamp())
            .map(|(id, _)| *id);
        if let Some(release_id) = target {
            missing.push((release_id, *cid));
        }
    }

    // Apply: additions are always auto; retractions are gated.
    let signer = profile
        .signer()
        .map_err(|e| Error::Node(node::Error::Usage(format!("signer: {e}"))))?;
    let mut report = RepoReport::default();

    for (release_id, cid) in missing {
        let url = iroh_url::build_from_id_str(ctx.endpoint_id).map_err(|e| {
            Error::Node(node::Error::Usage(format!(
                "invalid endpoint id from node: {e}"
            )))
        })?;
        let mut release_mut = releases
            .get_mut(&release_id)
            .map_err(|e| Error::Node(node::Error::Find(e)))?;
        match release_mut.add_location(cid, url, &signer) {
            Ok(_) => {
                eprintln!("added missing location for {cid} on {release_id}");
                report.added += 1;
            }
            Err(err) => {
                eprintln!("warning: failed to add location for {cid} on {release_id}: {err}")
            }
        }
    }

    for (release_id, cid, url) in orphaned_self {
        if ctx.retract_orphaned.contains(&cid) {
            let mut release_mut = releases
                .get_mut(&release_id)
                .map_err(|e| Error::Node(node::Error::Find(e)))?;
            match release_mut.remove_location(cid, url.clone(), &signer) {
                Ok(_) => {
                    eprintln!("retracted orphaned-self {cid} on {release_id}");
                    report.retracted += 1;
                }
                Err(err) => eprintln!("warning: failed to retract {url} on {release_id}: {err}"),
            }
        } else {
            eprintln!(
                "note: orphaned-self {cid} on {release_id} (pass `--retract-orphaned {cid}` to act)"
            );
            report.orphaned_self_skipped.push(cid);
        }
    }

    for (release_id, cid, url) in stale_endpoint {
        if ctx.retract_orphaned_self {
            let mut release_mut = releases
                .get_mut(&release_id)
                .map_err(|e| Error::Node(node::Error::Find(e)))?;
            match release_mut.remove_location(cid, url.clone(), &signer) {
                Ok(_) => {
                    eprintln!("retracted stale-endpoint {url} on {release_id}");
                    report.retracted += 1;
                }
                Err(err) => eprintln!("warning: failed to retract {url} on {release_id}: {err}"),
            }
        } else {
            report.stale_endpoint_skipped += 1;
        }
    }

    Ok(report)
}

fn print_summary(r: &RepoReport) {
    if r.added == 0
        && r.retracted == 0
        && r.orphaned_self_skipped.is_empty()
        && r.stale_endpoint_skipped == 0
    {
        eprintln!("Reconcile: no drift.");
        return;
    }
    if r.added > 0 {
        eprintln!("Reconcile: added {} missing location(s)", r.added);
    }
    if r.retracted > 0 {
        eprintln!("Reconcile: retracted {} stale location(s)", r.retracted);
    }
    if !r.orphaned_self_skipped.is_empty() {
        eprintln!(
            "Reconcile: {} orphaned-self CID(s) left in place (pass --retract-orphaned <CID>)",
            r.orphaned_self_skipped.len()
        );
    }
    if r.stale_endpoint_skipped > 0 {
        eprintln!(
            "Reconcile: {} stale-endpoint URL(s) left in place (pass --retract-orphaned-self)",
            r.stale_endpoint_skipped
        );
    }
}
