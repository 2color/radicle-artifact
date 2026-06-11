//! `rad-artifact reconcile` — fix COB drift relative to what the local
//! node is actively seeding.
//!
//! Three drift classes (per the design doc):
//! - **MissingLocation**: the node is seeding a `(rid, cid)` pair but
//!   no `radiroh://{current_endpoint}` location for that CID exists in
//!   any release under our DID. Auto-fixed by writing the location to
//!   the most recent matching release.
//! - **OrphanedSelf**: a `radiroh://{current_endpoint}` (or bare
//!   `radiroh://`, which resolves to our current endpoint) location
//!   under our DID exists for a CID we are *not* seeding. Flagged;
//!   removed either per-CID via `--remove-orphaned <CID>` or in bulk
//!   via `--remove-orphaned-self`.
//! - **StaleEndpoint**: a `radiroh://{other_endpoint}` location under
//!   our DID — i.e. one of our previous keys, or a malformed/legacy-encoded
//!   endpoint id that no longer decodes (e.g. a hex host left over from
//!   a previous encoding). Also catches pre-rename `iroh://` URLs, which
//!   no longer parse as endpoint URLs. Reported in summary; removed in
//!   bulk by `--remove-orphaned-self`.

use std::collections::HashSet;
use std::time::Duration;

use clap::Parser;
use radicle::{
    identity::{Did, RepoId},
    prelude::{Profile, ReadStorage},
    storage::git::Repository,
};
use radicle_artifact::Cid;
use radicle_artifact_client::{self as client, sync::Client};
use radicle_artifact_core::keys::EndpointId;
use radicle_artifact_core::protocol::{Command as NodeMsg, SeededEntry, Status};
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
    /// Remove our `radiroh://{current_endpoint}` location for this CID,
    /// which the node is no longer seeding. Repeatable.
    #[clap(long = "remove-orphaned", value_name = "CID")]
    pub remove_orphaned: Vec<Cid>,
    /// Remove every location under our DID that no longer reflects
    /// what the local node is seeding from its current endpoint:
    /// both orphaned-self entries (current endpoint, CID not in store)
    /// and stale-endpoint entries (URL pinned to a previous or
    /// undecodable endpoint id). Use after reviewing a previous
    /// `reconcile` run.
    #[clap(long, conflicts_with = "remove_orphaned")]
    pub remove_orphaned_self: bool,
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

/// One row per affected location, used both for per-action logging and
/// the final summary.
#[derive(Debug)]
struct LocationRow {
    rid: RepoId,
    release_id: radicle_artifact::ReleaseId,
    cid: Cid,
    url: Url,
}

/// A seeded tag with no release referencing its CID. Has no release or
/// URL, unlike [`LocationRow`].
#[derive(Debug)]
struct DanglingRow {
    rid: RepoId,
    cid: Cid,
}

#[derive(Default, Debug)]
struct RepoReport {
    added: u32,
    removed: u32,
    orphaned_self_skipped: Vec<LocationRow>,
    stale_endpoint_skipped: Vec<LocationRow>,
    dangling: Vec<DanglingRow>,
}

/// One endpoint URL under our DID, flattened out of the release/artifact
/// tree so classification can be unit-tested without a real COB. Includes
/// legacy `iroh://` URLs so the sweep can retract them.
struct OurLocation {
    release_id: radicle_artifact::ReleaseId,
    cid: Cid,
    url: Url,
}

/// One (release, cid) occurrence — every artifact entry across every
/// release, regardless of whether it has a location under our DID.
/// Used to pick the most recent matching release when filling in a
/// missing location.
struct ReleaseArtifact {
    release_id: radicle_artifact::ReleaseId,
    timestamp: u64,
    cid: Cid,
}

/// Result of [`classify_locations`].
#[derive(Default, Debug)]
struct Classified {
    /// CIDs that already have a current-endpoint location under our DID.
    current_endpoint_have: HashSet<Cid>,
    /// Current-endpoint locations under our DID whose CID the node is
    /// no longer seeding.
    orphaned_self: Vec<(radicle_artifact::ReleaseId, Cid, Url)>,
    /// Locations under our DID pinned to a different (or undecodable)
    /// endpoint id.
    stale_endpoint: Vec<(radicle_artifact::ReleaseId, Cid, Url)>,
}

/// Sort each endpoint URL under our DID into one of the three buckets.
///
/// - Bare `radiroh://` (no host) resolves to our current endpoint because
///   the host falls back to the location author's DID, which is us.
/// - A host that fails to decode is treated as stale: it isn't our
///   current endpoint id and belongs in the same removal bucket as
///   a foreign one. Legacy `iroh://` URLs land here too — `from_url`
///   rejects the scheme, so they fall into the stale bucket for sweep.
fn classify_locations(
    endpoint_id: EndpointId,
    seeded: &HashSet<Cid>,
    locations: impl IntoIterator<Item = OurLocation>,
) -> Classified {
    let mut out = Classified::default();
    for loc in locations {
        // A bare radiroh:// resolves to the location author's DID (us);
        // a hosted one must match our endpoint. Foreign, undecodable, or
        // legacy `iroh://` URLs don't match and fall to the stale bucket.
        if endpoint_id.matches_url(&loc.url) {
            out.current_endpoint_have.insert(loc.cid);
            if !seeded.contains(&loc.cid) {
                out.orphaned_self.push((loc.release_id, loc.cid, loc.url));
            }
        } else {
            out.stale_endpoint.push((loc.release_id, loc.cid, loc.url));
        }
    }
    out
}

/// Result of [`find_missing`]: seeded CIDs split by whether any release
/// can anchor a location for them.
#[derive(Default, Debug)]
struct MissingScan {
    /// Seeded CIDs we don't yet advertise, paired with the most recent
    /// release that contains them — a location can be added there.
    missing: Vec<(radicle_artifact::ReleaseId, Cid)>,
    /// Seeded CIDs that no release references at all. We have no anchor
    /// for a location, so these are reported as dangling tags rather
    /// than silently dropped.
    dangling: Vec<Cid>,
}

/// For every seeded CID that we don't already advertise from the
/// current endpoint, pick the most recent release that contains the
/// CID. CIDs no release references go to `dangling`.
fn find_missing(
    seeded: &HashSet<Cid>,
    current_have: &HashSet<Cid>,
    artifacts: &[ReleaseArtifact],
) -> MissingScan {
    let mut scan = MissingScan::default();
    for cid in seeded {
        if current_have.contains(cid) {
            continue;
        }
        let target = artifacts
            .iter()
            .filter(|r| &r.cid == cid)
            .max_by_key(|r| r.timestamp)
            .map(|r| r.release_id);
        match target {
            Some(release_id) => scan.missing.push((release_id, *cid)),
            None => scan.dangling.push(*cid),
        }
    }
    scan
}

/// Carries everything `reconcile_one` needs to inspect a single repo
/// and apply the chosen removal policy.
struct ReconcileCtx<'a> {
    client: &'a Client,
    endpoint_id: EndpointId,
    local_did: &'a Did,
    remove_orphaned: &'a HashSet<Cid>,
    remove_orphaned_self: bool,
}

/// Entry point for `rad-artifact reconcile`.
pub fn run(cli: Cli, repo_override: Option<RepoId>, profile: &Profile) -> Result<(), Error> {
    // We need the node up to know our current endpoint id and to ask
    // about seeded tags.
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    let status = client
        .call::<Status>(&NodeMsg::Status, client::DEFAULT_TIMEOUT)
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

    let remove_orphaned: HashSet<Cid> = cli.remove_orphaned.iter().copied().collect();
    let ctx = ReconcileCtx {
        client: &client,
        endpoint_id,
        local_did: &local_did,
        remove_orphaned: &remove_orphaned,
        remove_orphaned_self: cli.remove_orphaned_self,
    };
    let mut total = RepoReport::default();
    let mut failed: usize = 0;

    for rid in target_rids {
        let report = profile
            .storage
            .repository(rid)
            .map_err(|e| Error::Repository {
                rid,
                err: e.to_string(),
            })
            .and_then(|repo| reconcile_one(rid, &repo, &ctx, profile));
        match report {
            Ok(report) => {
                total.added += report.added;
                total.removed += report.removed;
                total
                    .orphaned_self_skipped
                    .extend(report.orphaned_self_skipped);
                total
                    .stale_endpoint_skipped
                    .extend(report.stale_endpoint_skipped);
                total.dangling.extend(report.dangling);
            }
            // In a bulk run one broken repo (e.g. no releases COB) must
            // not block the rest; surface it and carry on. A single
            // explicit target still fails fast.
            Err(e) if cli.all_repos => {
                failed += 1;
                eprintln!("warning: skipping {rid}: {e}");
            }
            Err(e) => return Err(e),
        }
    }

    print_summary(&total);
    if failed > 0 {
        eprintln!("{failed} repo(s) could not be reconciled (see warnings above)");
    }
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
        .call(&NodeMsg::ListSeeded { rid }, Duration::from_secs(30))
        .map_err(|e| Error::Node(node::client_err(e)))?;
    let seeded: HashSet<Cid> = entries.into_iter().map(|e| e.cid).collect();

    // Snapshot every release once so the loop body can drop its borrow
    // before mutations begin.
    let all_releases: Vec<(radicle_artifact::ReleaseId, radicle_artifact::Release)> = releases
        .all()
        .map_err(|e| Error::Node(node::Error::Find(e)))?
        .filter_map(|r| r.ok())
        .map(|(oid, r)| (radicle_artifact::ReleaseId::from(oid), r))
        .collect();

    // Build flat snapshots so classification can be tested in isolation.
    let mut our_locations: Vec<OurLocation> = Vec::new();
    let mut all_artifacts: Vec<ReleaseArtifact> = Vec::new();
    for (release_id, release) in &all_releases {
        for (cid, artifact) in release.artifacts() {
            all_artifacts.push(ReleaseArtifact {
                release_id: *release_id,
                timestamp: release.timestamp(),
                cid: *cid,
            });
            let Some(urls) = artifact.locations_of(ctx.local_did) else {
                continue;
            };
            // Legacy `iroh://` URLs are collected too so the sweep can
            // retract them; classify_locations sorts them into stale_endpoint.
            for url in urls
                .iter()
                .filter(|u| EndpointId::is_endpoint_url(u) || EndpointId::is_legacy_endpoint_url(u))
            {
                our_locations.push(OurLocation {
                    release_id: *release_id,
                    cid: *cid,
                    url: url.clone(),
                });
            }
        }
    }

    let Classified {
        current_endpoint_have,
        orphaned_self,
        stale_endpoint,
    } = classify_locations(ctx.endpoint_id, &seeded, our_locations);
    let MissingScan { missing, dangling } =
        find_missing(&seeded, &current_endpoint_have, &all_artifacts);

    // Apply: additions are always auto; removals are gated.
    let signer = profile
        .signer()
        .map_err(|e| Error::Node(node::Error::Usage(format!("signer: {e}"))))?;

    // Dangling tags have no release to anchor a location to; report them
    // so the user can reclaim the bytes with `rad-artifact unseed --cid <cid>`.
    let mut report = RepoReport {
        dangling: dangling
            .into_iter()
            .map(|cid| DanglingRow { rid, cid })
            .collect(),
        ..Default::default()
    };

    for (release_id, cid) in missing {
        // ctx.endpoint_id is already validated; build its canonical URL.
        let url = ctx.endpoint_id.to_url();
        let mut release_mut = releases
            .get_mut(&release_id)
            .map_err(|e| Error::Node(node::Error::Find(e)))?;
        match release_mut.add_location(cid, url.clone(), &signer) {
            Ok(_) => {
                eprintln!("added: rid={rid} release={release_id} cid={cid} url={url}");
                report.added += 1;
            }
            Err(err) => {
                eprintln!(
                    "warning: failed to add rid={rid} release={release_id} cid={cid} url={url}: {err}"
                );
            }
        }
    }

    for (release_id, cid, url) in orphaned_self {
        if ctx.remove_orphaned_self || ctx.remove_orphaned.contains(&cid) {
            let mut release_mut = releases
                .get_mut(&release_id)
                .map_err(|e| Error::Node(node::Error::Find(e)))?;
            match release_mut.remove_location(cid, url.clone(), &signer) {
                Ok(_) => {
                    eprintln!(
                        "removed orphaned-self: rid={rid} release={release_id} cid={cid} url={url}"
                    );
                    report.removed += 1;
                }
                Err(err) => eprintln!(
                    "warning: failed to remove rid={rid} release={release_id} cid={cid} url={url}: {err}"
                ),
            }
        } else {
            // Skipped rows are surfaced once in the final summary
            // instead of also being logged here.
            report.orphaned_self_skipped.push(LocationRow {
                rid,
                release_id,
                cid,
                url,
            });
        }
    }

    for (release_id, cid, url) in stale_endpoint {
        if ctx.remove_orphaned_self {
            let mut release_mut = releases
                .get_mut(&release_id)
                .map_err(|e| Error::Node(node::Error::Find(e)))?;
            match release_mut.remove_location(cid, url.clone(), &signer) {
                Ok(_) => {
                    eprintln!(
                        "removed stale-endpoint: rid={rid} release={release_id} cid={cid} url={url}"
                    );
                    report.removed += 1;
                }
                Err(err) => eprintln!(
                    "warning: failed to remove rid={rid} release={release_id} cid={cid} url={url}: {err}"
                ),
            }
        } else {
            report.stale_endpoint_skipped.push(LocationRow {
                rid,
                release_id,
                cid,
                url,
            });
        }
    }

    Ok(report)
}

fn print_summary(r: &RepoReport) {
    if r.added == 0
        && r.removed == 0
        && r.orphaned_self_skipped.is_empty()
        && r.stale_endpoint_skipped.is_empty()
        && r.dangling.is_empty()
    {
        eprintln!("Reconcile: no drift.");
        return;
    }
    if r.added > 0 {
        eprintln!("Reconcile: added {} missing location(s)", r.added);
    }
    if r.removed > 0 {
        eprintln!("Reconcile: removed {} stale location(s)", r.removed);
    }
    if !r.orphaned_self_skipped.is_empty() {
        eprintln!();
        eprintln!(
            "Reconcile: {} orphaned-self location(s) left in place — pass --remove-orphaned <CID> (or --remove-orphaned-self for all) to remove:",
            r.orphaned_self_skipped.len()
        );
        print_grouped_by_rid(&r.orphaned_self_skipped);
    }
    if !r.stale_endpoint_skipped.is_empty() {
        eprintln!();
        eprintln!(
            "Reconcile: {} stale-endpoint location(s) left in place — pass --remove-orphaned-self to remove (also covers orphaned-self):",
            r.stale_endpoint_skipped.len()
        );
        print_grouped_by_rid(&r.stale_endpoint_skipped);
    }
    if !r.dangling.is_empty() {
        eprintln!();
        eprintln!(
            "Reconcile: {} dangling tag(s) — seeded but no release references them; run `rad-artifact unseed --cid <cid>` to reclaim:",
            r.dangling.len()
        );
        print_dangling_by_rid(&r.dangling);
    }
}

/// Print dangling tags grouped by RID, each CID on its own indented line.
fn print_dangling_by_rid(rows: &[DanglingRow]) {
    use std::collections::BTreeMap;
    let mut by_rid: BTreeMap<String, Vec<&DanglingRow>> = BTreeMap::new();
    for row in rows {
        by_rid.entry(row.rid.to_string()).or_default().push(row);
    }
    for (rid, items) in &by_rid {
        eprintln!("  {rid}");
        for row in items {
            eprintln!("    cid: {}", row.cid);
        }
    }
}

/// Print skipped locations grouped by RID, then release, with each CID
/// and URL on its own indented line.
fn print_grouped_by_rid(rows: &[LocationRow]) {
    use std::collections::BTreeMap;
    let mut by_rid: BTreeMap<String, Vec<&LocationRow>> = BTreeMap::new();
    for row in rows {
        by_rid.entry(row.rid.to_string()).or_default().push(row);
    }
    for (rid, items) in &by_rid {
        eprintln!("  {rid}");
        for row in items {
            eprintln!("    release {}", row.release_id);
            eprintln!("      cid: {}", row.cid);
            eprintln!("      url: {}", row.url);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashSet;
    use std::str::FromStr;

    use radicle_artifact::{Cid, ReleaseId};
    use radicle_artifact_core::cid::{blake3_hash_to_cid, ArtifactKind};
    use radicle_artifact_core::keys::EndpointId;
    use url::Url;

    use super::{classify_locations, find_missing, OurLocation, ReleaseArtifact};

    // `find_missing` returns `MissingScan { missing, dangling }`.

    /// Production-faithful CID: BLAKE3 multihash, raw codec, derived
    /// from a single distinguishing byte.
    fn test_cid(n: u8) -> Cid {
        blake3_hash_to_cid(blake3::hash(&[n]), ArtifactKind::Blob)
    }

    /// 40-char hex Oid keyed by `n`.
    fn test_release(n: u8) -> ReleaseId {
        ReleaseId::from_str(&format!("{n:040x}")).unwrap()
    }

    /// Endpoint id derived from a fixed-byte secret.
    fn test_endpoint(byte: u8) -> EndpointId {
        iroh_base::SecretKey::from_bytes(&[byte; 32])
            .public()
            .into()
    }

    fn bare_iroh_url() -> Url {
        Url::parse(&format!("{}://", EndpointId::URL_SCHEME)).unwrap()
    }

    fn undecodable_iroh_url() -> Url {
        // '1' is not in the base32 alphabet (a-z + 2-7).
        Url::parse(&format!("{}://abc123", EndpointId::URL_SCHEME)).unwrap()
    }

    /// A pre-rename `iroh://` URL with our own (correctly-encoded) host.
    fn legacy_iroh_url(ep: EndpointId) -> Url {
        // Same base32 host the old scheme used; only the scheme differs.
        let host = ep.to_url().host_str().unwrap().to_string();
        Url::parse(&format!("iroh://{host}")).unwrap()
    }

    // --- classify_locations -------------------------------------------------

    #[test]
    fn bare_iroh_is_current_endpoint() {
        let our_ep = test_endpoint(1);
        let cid = test_cid(1);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let out = classify_locations(
            our_ep,
            &seeded,
            [OurLocation {
                release_id: test_release(1),
                cid,
                url: bare_iroh_url(),
            }],
        );
        assert!(out.current_endpoint_have.contains(&cid));
        assert!(out.orphaned_self.is_empty());
        assert!(out.stale_endpoint.is_empty());
    }

    #[test]
    fn explicit_current_endpoint_is_current() {
        let our_ep = test_endpoint(1);
        let cid = test_cid(1);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let out = classify_locations(
            our_ep,
            &seeded,
            [OurLocation {
                release_id: test_release(1),
                cid,
                url: our_ep.to_url(),
            }],
        );
        assert!(out.current_endpoint_have.contains(&cid));
        assert!(out.orphaned_self.is_empty());
        assert!(out.stale_endpoint.is_empty());
    }

    #[test]
    fn other_endpoint_is_stale() {
        let our_ep = test_endpoint(1);
        let other_ep = test_endpoint(2);
        let cid = test_cid(1);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let out = classify_locations(
            our_ep,
            &seeded,
            [OurLocation {
                release_id: test_release(1),
                cid,
                url: other_ep.to_url(),
            }],
        );
        assert!(!out.current_endpoint_have.contains(&cid));
        assert!(out.orphaned_self.is_empty());
        assert_eq!(out.stale_endpoint.len(), 1);
    }

    #[test]
    fn undecodable_host_is_stale() {
        // Regression: hosts that fail base32 decoding (e.g. legacy hex
        // endpoint ids) must go to the stale bucket so
        // --remove-orphaned-self can clean them up.
        let our_ep = test_endpoint(1);
        let cid = test_cid(1);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let out = classify_locations(
            our_ep,
            &seeded,
            [OurLocation {
                release_id: test_release(1),
                cid,
                url: undecodable_iroh_url(),
            }],
        );
        assert!(!out.current_endpoint_have.contains(&cid));
        assert!(out.orphaned_self.is_empty());
        assert_eq!(out.stale_endpoint.len(), 1);
    }

    #[test]
    fn current_endpoint_not_seeded_is_orphaned_self() {
        let our_ep = test_endpoint(1);
        let cid = test_cid(1);
        let seeded: HashSet<Cid> = HashSet::new();
        let out = classify_locations(
            our_ep,
            &seeded,
            [OurLocation {
                release_id: test_release(1),
                cid,
                url: our_ep.to_url(),
            }],
        );
        assert!(out.current_endpoint_have.contains(&cid));
        assert_eq!(out.orphaned_self.len(), 1);
        assert!(out.stale_endpoint.is_empty());
    }

    #[test]
    fn mixed_urls_on_same_cid_split_into_buckets() {
        // One CID with two URLs: one current, one stale. The current
        // URL marks the CID already-advertised; the stale URL still
        // goes to the stale bucket so it can be removed independently.
        let our_ep = test_endpoint(1);
        let other_ep = test_endpoint(2);
        let cid = test_cid(1);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let rel = test_release(1);
        let out = classify_locations(
            our_ep,
            &seeded,
            [
                OurLocation {
                    release_id: rel,
                    cid,
                    url: our_ep.to_url(),
                },
                OurLocation {
                    release_id: rel,
                    cid,
                    url: other_ep.to_url(),
                },
            ],
        );
        assert!(out.current_endpoint_have.contains(&cid));
        assert!(out.orphaned_self.is_empty());
        assert_eq!(out.stale_endpoint.len(), 1);
    }

    #[test]
    fn legacy_iroh_scheme_is_stale() {
        // Migration sweep: a pre-rename `iroh://` URL under our DID no
        // longer parses as an endpoint URL, so it lands in the stale
        // bucket and --remove-orphaned-self retracts it.
        let our_ep = test_endpoint(1);
        let cid = test_cid(1);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let out = classify_locations(
            our_ep,
            &seeded,
            [OurLocation {
                release_id: test_release(1),
                cid,
                // Legacy URL pinned to our *own* current endpoint id: even
                // though the host matches, the scheme is stale.
                url: legacy_iroh_url(our_ep),
            }],
        );
        assert!(!out.current_endpoint_have.contains(&cid));
        assert!(out.orphaned_self.is_empty());
        assert_eq!(out.stale_endpoint.len(), 1);
    }

    #[test]
    fn legacy_only_cid_is_reported_missing() {
        // After the sweep retracts a CID's only (legacy) location, the
        // CID has no current-endpoint location, so find_missing reports
        // it — that's the path that re-adds a fresh `radiroh://` URL.
        let our_ep = test_endpoint(1);
        let cid = test_cid(1);
        let release = test_release(1);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let classified = classify_locations(
            our_ep,
            &seeded,
            [OurLocation {
                release_id: release,
                cid,
                url: legacy_iroh_url(our_ep),
            }],
        );
        // Legacy URL did not count as a current-endpoint location.
        let artifacts = vec![ReleaseArtifact {
            release_id: release,
            timestamp: 100,
            cid,
        }];
        let scan = find_missing(&seeded, &classified.current_endpoint_have, &artifacts);
        assert_eq!(scan.missing, vec![(release, cid)]);
    }

    // --- find_missing -------------------------------------------------------

    #[test]
    fn missing_picks_release_with_latest_timestamp() {
        let cid = test_cid(1);
        let older = test_release(1);
        let newer = test_release(2);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let have: HashSet<Cid> = HashSet::new();
        let artifacts = vec![
            ReleaseArtifact {
                release_id: older,
                timestamp: 100,
                cid,
            },
            ReleaseArtifact {
                release_id: newer,
                timestamp: 200,
                cid,
            },
        ];
        let scan = find_missing(&seeded, &have, &artifacts);
        assert_eq!(scan.missing, vec![(newer, cid)]);
        assert!(scan.dangling.is_empty());
    }

    #[test]
    fn seeded_with_current_location_is_not_missing() {
        let cid = test_cid(1);
        let seeded: HashSet<Cid> = [cid].into_iter().collect();
        let have: HashSet<Cid> = [cid].into_iter().collect();
        let artifacts = vec![ReleaseArtifact {
            release_id: test_release(1),
            timestamp: 100,
            cid,
        }];
        let scan = find_missing(&seeded, &have, &artifacts);
        assert!(scan.missing.is_empty());
        assert!(scan.dangling.is_empty());
    }

    #[test]
    fn seeded_cid_with_no_matching_release_is_dangling() {
        // Seeded CID isn't present in any release, so reconcile has no
        // anchor for a missing-location add. Report it as dangling
        // rather than dropping it silently.
        let seeded_cid = test_cid(1);
        let other_cid = test_cid(2);
        let seeded: HashSet<Cid> = [seeded_cid].into_iter().collect();
        let have: HashSet<Cid> = HashSet::new();
        let artifacts = vec![ReleaseArtifact {
            release_id: test_release(1),
            timestamp: 100,
            cid: other_cid,
        }];
        let scan = find_missing(&seeded, &have, &artifacts);
        assert!(scan.missing.is_empty());
        assert_eq!(scan.dangling, vec![seeded_cid]);
    }

    #[test]
    fn missing_and_dangling_split_correctly() {
        // One seeded CID lives in a release (missing-location add), the
        // other lives in none (dangling). They must route to different
        // buckets.
        let anchored = test_cid(1);
        let orphan = test_cid(2);
        let release = test_release(1);
        let seeded: HashSet<Cid> = [anchored, orphan].into_iter().collect();
        let have: HashSet<Cid> = HashSet::new();
        let artifacts = vec![ReleaseArtifact {
            release_id: release,
            timestamp: 100,
            cid: anchored,
        }];
        let scan = find_missing(&seeded, &have, &artifacts);
        assert_eq!(scan.missing, vec![(release, anchored)]);
        assert_eq!(scan.dangling, vec![orphan]);
    }
}
