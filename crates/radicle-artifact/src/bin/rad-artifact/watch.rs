//! `rad-artifact watch`: keep a node seeding the artifacts its trusted
//! peers publish.
//!
//! The loop has two halves. The Radicle node's event stream says when a
//! repository's COBs moved, so a newly registered artifact is picked up
//! within seconds; a full sweep every `--sweep` seconds catches anything
//! that landed while this process, or the Radicle node, was down. The
//! sweep runs on a wall-clock deadline rather than on stream silence — a
//! chatty node must not be able to starve it.
//!
//! It lives in the CLI rather than in `rad-artifact-node` because seeding
//! an artifact also means Adding a `radiroh://` Location, and that is a
//! signed COB write — the seeding node writes no COBs.
//!
//! Failures are isolated: one unreadable repository or one unfetchable
//! artifact is a warning, and only losing the subscription backs off.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

use clap::Parser;
use radicle::identity::{Did, RepoId};
use radicle::node::{Handle, Node};
use radicle::prelude::{Profile, ReadStorage};
use radicle::storage::{git::Repository, RefUpdate};
use radicle_artifact::display::human_bytes;
use radicle_artifact::watch::{fits, wanted, Wanted};
use radicle_artifact::{Cid, Release, ReleaseId, TYPENAME};
use radicle_artifact_client::{sync::Client, FetchArgs};
use radicle_artifact_core::keys::EndpointId;
use radicle_artifact_core::protocol::FetchLocation;
use thiserror::Error;

use crate::{
    add_seed_location, announce, artifact_locations, node, open_releases, FETCH_IDLE_TIMEOUT,
};

/// How long to wait before re-subscribing after the Radicle node's event
/// stream drops. Short enough that a node restart is barely noticed,
/// long enough not to spin when the node stays down.
const RECONNECT_DELAY: Duration = Duration::from_secs(10);

/// `rad-artifact watch` clap entry.
#[derive(Parser)]
pub struct Cli {
    /// Repositories to watch. Defaults to every repository the Radicle
    /// node seeds.
    pub rids: Vec<RepoId>,
    /// Stop seeding once the node's store holds this much, e.g. `50G`.
    /// Unbounded when omitted.
    #[clap(long, value_parser = parse_size)]
    pub budget: Option<u64>,
    /// Seconds between full sweeps over every watched repository.
    #[clap(long, default_value_t = 900)]
    pub sweep: u64,
    /// Report what would be seeded, and fetch nothing.
    #[clap(long)]
    pub dry_run: bool,
}

/// Failure modes for `rad-artifact watch`.
#[derive(Debug, Error)]
pub enum Error {
    /// Usage / argument validation error.
    #[error("{0}")]
    Usage(String),
    /// A named repository is not seeded by the Radicle node.
    #[error("{0} is not seeded by your radicle node\n  hint: seed it first with `rad seed {0}`")]
    NotSeeded(RepoId),
    /// Could not read the Radicle node's seeding policies.
    #[error("failed to read the radicle node's seeding policies")]
    Policies(#[source] radicle::node::policy::store::Error),
    /// Could not enumerate local storage.
    #[error("failed to read local storage: {0}")]
    Storage(String),
    /// Could not open a repository.
    #[error("failed to open repository {rid}: {err}")]
    Repository {
        /// Repository that could not be opened.
        rid: RepoId,
        /// Underlying failure, rendered.
        err: String,
    },
    /// Failure talking to the seeding node.
    #[error(transparent)]
    Node(#[from] node::Error),
    /// Failure resolving an artifact's locations or writing its location.
    #[error("{0}")]
    Artifact(String),
    /// Failed to announce the COB write after seeding.
    #[error(transparent)]
    Announce(#[from] crate::error::Announce),
}

/// Everything the per-artifact work needs that does not change between
/// repositories.
struct Ctx<'a> {
    client: &'a Client,
    /// Our own endpoint, both the Location we publish and the provider we
    /// must never try to fetch from.
    endpoint_id: EndpointId,
    local: Did,
    budget: Option<u64>,
    dry_run: bool,
    no_announce: bool,
    /// Repositories whose COB writes are not announced yet. An artifact is
    /// seeded once and then skipped as seeded, so a lost announcement never
    /// comes back on its own: hold the repository here until one succeeds.
    unannounced: RefCell<BTreeSet<RepoId>>,
    /// Messages already printed. A steady state — an artifact with no
    /// location, or a full store — recurs on every sweep, and a supervised
    /// process must not log it for its whole life.
    said: RefCell<HashSet<String>>,
}

impl Ctx<'_> {
    /// Print `message` the first time it comes up. A message that changes,
    /// such as one carrying a new count or total, is new again.
    fn say_once(&self, message: String) {
        if self.said.borrow_mut().insert(message.clone()) {
            eprintln!("{message}");
        }
    }
}

/// Why the watch loop came back for another sweep.
enum Tick {
    /// The sweep interval elapsed with the subscription healthy.
    Elapsed,
    /// The subscription could not be opened or dropped mid-stream.
    Lost,
}

/// Entry point for `rad-artifact watch`.
pub fn run(cli: Cli, no_announce: bool, profile: &Profile) -> Result<(), Error> {
    // The node has to be up: it holds the bytes, and its endpoint id is
    // the Location we publish.
    let client = Client::new(Client::default_socket(profile.home.path()));
    let status = client
        .status()
        .map_err(|e| Error::Node(node::client_err(e)))?;

    let only: BTreeSet<RepoId> = cli.rids.iter().copied().collect();
    // Fail fast on a named repository the Radicle node does not seed —
    // its COBs would never arrive, so watching it would be silent.
    for rid in &only {
        if !is_seeded(*rid, profile)? {
            return Err(Error::NotSeeded(*rid));
        }
    }

    let ctx = Ctx {
        client: &client,
        endpoint_id: status.endpoint_id,
        local: Did::from(*profile.id()),
        budget: cli.budget,
        dry_run: cli.dry_run,
        no_announce,
        unannounced: RefCell::new(BTreeSet::new()),
        said: RefCell::new(HashSet::new()),
    };
    let sweep = Duration::from_secs(cli.sweep.max(1));
    let node = Node::new(profile.home.socket_from_env());

    match ctx.budget {
        Some(limit) => eprintln!(
            "Watching for trusted artifacts (budget {})",
            human_bytes(limit)
        ),
        None => eprintln!("Watching for trusted artifacts"),
    }

    // Due immediately, so a fresh start catches up before it waits on
    // anything. Keeping the deadline outside the loop is what stops a
    // node that is down from turning `RECONNECT_DELAY` into the sweep
    // interval: reconnecting does not earn another sweep.
    let mut next_sweep = Instant::now();
    loop {
        if Instant::now() >= next_sweep {
            // A storage read that fails here is a warning: the next sweep
            // retries, and the event stream keeps working meanwhile.
            match targets(&only, profile) {
                Ok(rids) => {
                    for rid in rids {
                        process(rid, &ctx, profile);
                    }
                }
                Err(e) => eprintln!("warning: cannot list repositories to watch: {e}"),
            }
            // Retry announcements a repository of its own never revisits.
            flush_announcements(&ctx, profile);
            next_sweep = Instant::now() + sweep;
        }
        // Read events until the sweep falls due, so a silent stream times
        // out exactly when it is needed.
        let until_sweep = next_sweep
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(1));
        let tick = match node.subscribe(until_sweep) {
            Ok(events) => drain(events, next_sweep, &only, &ctx, profile),
            Err(e) => {
                eprintln!("warning: cannot reach the radicle node: {e}");
                Tick::Lost
            }
        };
        if matches!(tick, Tick::Lost) {
            std::thread::sleep(RECONNECT_DELAY);
        }
    }
}

/// Announce the repositories whose COB writes are not out yet, keeping any
/// that still fail for the next attempt.
fn flush_announcements(ctx: &Ctx<'_>, profile: &Profile) {
    let pending: Vec<RepoId> = ctx.unannounced.borrow().iter().copied().collect();
    for rid in pending {
        match announce(profile, rid) {
            Ok(()) => {
                ctx.unannounced.borrow_mut().remove(&rid);
            }
            Err(e) => eprintln!("warning: {rid}: cannot announce yet, will retry: {e}"),
        }
    }
}

/// Read events until `deadline`, or until the stream drops.
///
/// The subscription's read timeout is the time left until `deadline`, so a
/// silent stream yields `TimedOut` when the next sweep is due. A busy one
/// never times out, hence the explicit deadline: the tick and the
/// subscription share one thread, and the tick has to win.
fn drain(
    events: impl Iterator<Item = Result<radicle::node::Event, radicle::node::Error>>,
    deadline: Instant,
    only: &BTreeSet<RepoId>,
    ctx: &Ctx<'_>,
    profile: &Profile,
) -> Tick {
    for event in events {
        if Instant::now() >= deadline {
            return Tick::Elapsed;
        }
        match event {
            Ok(radicle::node::Event::RefsFetched { rid, updated, .. }) => {
                if !touches_artifacts(&updated) {
                    continue;
                }
                if !only.is_empty() && !only.contains(&rid) {
                    continue;
                }
                match is_seeded(rid, profile) {
                    Ok(true) => process(rid, ctx, profile),
                    Ok(false) => {}
                    Err(e) => eprintln!("warning: {rid}: {e}"),
                }
            }
            Ok(_) => {}
            Err(radicle::node::Error::TimedOut) => return Tick::Elapsed,
            Err(e) => {
                eprintln!("warning: lost the radicle node's event stream: {e}");
                return Tick::Lost;
            }
        }
    }
    Tick::Lost
}

/// Whether any updated ref belongs to an artifact release COB.
fn touches_artifacts(updated: &[RefUpdate]) -> bool {
    let needle = format!("/cobs/{}/", *TYPENAME);
    updated.iter().any(|u| ref_name(u).contains(&needle))
}

fn ref_name(update: &RefUpdate) -> &str {
    match update {
        RefUpdate::Updated { name, .. }
        | RefUpdate::Created { name, .. }
        | RefUpdate::Deleted { name, .. }
        | RefUpdate::Skipped { name, .. } => name.as_str(),
    }
}

/// The repositories a sweep covers: the named ones, else every
/// repository the Radicle node seeds.
fn targets(only: &BTreeSet<RepoId>, profile: &Profile) -> Result<Vec<RepoId>, Error> {
    if !only.is_empty() {
        return Ok(only.iter().copied().collect());
    }
    let mut rids = Vec::new();
    for info in profile
        .storage
        .repositories()
        .map_err(|e| Error::Storage(e.to_string()))?
    {
        if is_seeded(info.rid, profile)? {
            rids.push(info.rid);
        }
    }
    Ok(rids)
}

/// Trust in a repository is the Radicle node's own seeding policy: if you
/// seed the repo's git, you seed its artifacts.
fn is_seeded(rid: RepoId, profile: &Profile) -> Result<bool, Error> {
    profile
        .policies()
        .map_err(Error::Policies)?
        .is_seeding(&rid)
        .map_err(Error::Policies)
}

/// Seed everything trusted and unseeded in one repository. A repository
/// that cannot be read is a warning, never fatal — the next sweep retries.
fn process(rid: RepoId, ctx: &Ctx<'_>, profile: &Profile) {
    if let Err(e) = process_repo(rid, ctx, profile) {
        eprintln!("warning: {rid}: {e}");
    }
}

fn process_repo(rid: RepoId, ctx: &Ctx<'_>, profile: &Profile) -> Result<(), Error> {
    let repo = profile
        .storage
        .repository(rid)
        .map_err(|e| Error::Repository {
            rid,
            err: e.to_string(),
        })?;

    // Snapshot the releases so the store borrow ends before we write
    // Locations back into it.
    let (delegates, releases): (_, Vec<(ReleaseId, Release)>) = {
        let store = open_releases(&repo, profile).map_err(|e| Error::Usage(e.to_string()))?;
        let releases = store
            .all()
            .map_err(|e| Error::Usage(e.to_string()))?
            .into_iter()
            .filter_map(Result::ok)
            .map(|(oid, release)| (ReleaseId::from(oid), release))
            .collect();
        (store.delegates().clone(), releases)
    };

    // One question for the whole repo rather than one per CID.
    let seeded: HashSet<Cid> = ctx
        .client
        .list_seeded(rid)
        .map_err(|e| Error::Node(node::client_err(e)))?
        .into_iter()
        .map(|entry| entry.cid)
        .collect();

    let wanted = wanted(&releases, &delegates, &ctx.local, |cid| {
        seeded.contains(cid)
    });
    if wanted.is_empty() {
        return Ok(());
    }

    // A full store is the common steady state once a budget is set, so say
    // it once per repository rather than once per artifact. Without a
    // budget there is nothing to compare against, so the node is not asked.
    if let Some(limit) = ctx.budget {
        let seeded_bytes = store_size(ctx)?;
        if seeded_bytes >= limit {
            ctx.say_once(format!(
                "{rid}: holding back {} artifact(s), {} already seeded",
                wanted.len(),
                human_bytes(seeded_bytes),
            ));
            return Ok(());
        }
    }

    let mut wrote = false;
    for want in wanted {
        match seed(rid, &repo, &releases, &want, ctx, profile) {
            Ok(added) => wrote |= added,
            Err(e) => eprintln!("warning: {} ({}): {e}", want.name, want.cid),
        }
    }
    // One announcement for the repository rather than one per artifact, and
    // never charged to an artifact that was seeded and recorded fine.
    if wrote && !ctx.no_announce {
        ctx.unannounced.borrow_mut().insert(rid);
        flush_announcements(ctx, profile);
    }
    Ok(())
}

/// Bytes the node's store currently holds across every seeded tag.
fn store_size(ctx: &Ctx<'_>) -> Result<u64, Error> {
    Ok(ctx
        .client
        .status()
        .map_err(|e| Error::Node(node::client_err(e)))?
        .seeded
        .bytes_logical)
}

/// Fetch one artifact into the node's store, tag it as seeded, and
/// publish our Location for it. Reports whether a Location was written, so
/// the caller announces once for the repository.
fn seed(
    rid: RepoId,
    repo: &Repository,
    releases: &[(ReleaseId, Release)],
    want: &Wanted,
    ctx: &Ctx<'_>,
    profile: &Profile,
) -> Result<bool, Error> {
    // Re-read the store size per artifact: the hints are optional, so
    // only the actual total keeps the budget honest as it fills.
    if let Some(limit) = ctx.budget {
        let seeded_bytes = store_size(ctx)?;
        if !fits(Some(limit), seeded_bytes, want.size_hint) {
            ctx.say_once(format!(
                "skipped {} ({}): over the budget, {} already seeded",
                want.name,
                want.cid,
                human_bytes(seeded_bytes),
            ));
            return Ok(false);
        }
    }

    // Union the locations across every release carrying this CID, then
    // drop ourselves — fetching from our own endpoint cannot work.
    let artifacts = releases.iter().filter_map(|(_, r)| r.artifact(&want.cid));
    let locations: Vec<FetchLocation> = artifact_locations(artifacts)
        .map_err(|e| Error::Artifact(e.to_string()))?
        .into_iter()
        .filter(|l| !matches!(l, FetchLocation::Iroh(id) if *id == ctx.endpoint_id))
        .collect();
    if locations.is_empty() {
        ctx.say_once(format!(
            "skipped {} ({}): no locations to fetch from",
            want.name, want.cid
        ));
        return Ok(false);
    }

    if ctx.dry_run {
        ctx.say_once(format!(
            "would seed {} ({}) from {} location(s)",
            want.name,
            want.cid,
            locations.len()
        ));
        return Ok(false);
    }

    ctx.client
        .fetch(
            FetchArgs {
                rid,
                cid: want.cid,
                locations,
                seed: Some(want.release_id.oid()),
            },
            FETCH_IDLE_TIMEOUT,
            |_| {},
        )
        .map_err(|e| Error::Node(node::client_err(e)))?;

    // The bytes are in the store now, so the next sweep counts this CID as
    // seeded and never comes back: point at `reconcile`, which repairs
    // exactly this drift.
    add_seed_location(ctx.endpoint_id, want.cid, want.release_id, repo, profile).map_err(|e| {
        Error::Artifact(format!(
            "{e}\n  hint: the bytes are seeded; `rad-artifact reconcile` adds the missing location"
        ))
    })?;
    Ok(true)
}

/// Parse a byte budget: a plain count, or one suffixed `K`, `M`, `G` or
/// `T` (binary multiples, matching how the node reports sizes).
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (digits, multiplier) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let m = match c.to_ascii_uppercase() {
                'K' => 1024u64,
                'M' => 1024 * 1024,
                'G' => 1024 * 1024 * 1024,
                'T' => 1024 * 1024 * 1024 * 1024,
                _ => return Err(format!("unknown size suffix '{c}', expected K, M, G or T")),
            };
            (&s[..s.len() - c.len_utf8()], m)
        }
        _ => (s, 1),
    };
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("'{s}' is not a byte size, e.g. 50G"))?;
    n.checked_mul(multiplier)
        .ok_or_else(|| format!("'{s}' is too large"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn plain_counts_and_suffixes_parse() {
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(parse_size("1K"), Ok(1024));
        assert_eq!(parse_size("2g"), Ok(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn unknown_suffixes_and_junk_are_rejected() {
        assert!(parse_size("5X").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("20000000T").is_err());
    }
}
