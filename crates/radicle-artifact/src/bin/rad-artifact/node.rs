//! `rad-artifact node` subcommand: control the local seeder daemon.
//!
//! Talks to the long-running `rad-artifact-node` daemon over the Unix
//! control socket. Subcommands that need a repository (list, seed,
//! unseed) honor the global `--repo <RID>` override or fall back to the
//! cwd's radicle repo. Subcommands that don't (start, stop, status,
//! logs) skip repo discovery entirely so they work from any directory.
//!
//! Pretty-print helpers (`print_status_pretty`, `humanize_uptime`) live
//! here too — they're node-specific UI. Byte sizes use the shared
//! [`radicle_artifact::display::human_bytes`].

use std::io::IsTerminal;
use std::time::Duration;

use clap::Parser;
use radicle::cob;
use radicle::{
    identity::{Did, RepoId},
    prelude::Profile,
    storage::git::Repository,
};
use radicle_artifact::display::human_bytes;
use radicle_artifact::lifecycle;
use radicle_artifact::{Cid, ReleaseId, Releases};
use radicle_artifact_client::{self as client, sync::Client, ClientError};
use radicle_artifact_core::cid as share;
use radicle_artifact_core::keys::EndpointId;
use radicle_artifact_core::protocol::{
    Command as NodeMsg, ImportMode, RelayStats, SeedReceipt, SeededEntry, Status, UnseedReceipt,
};
use thiserror::Error;
use url::Url;

use crate::{open_releases, open_repo, parse_release_id};

/// `rad-artifact node` clap entry.
#[derive(Parser)]
pub struct Cli {
    #[clap(subcommand)]
    pub command: Subcommand,
}

#[derive(Parser)]
pub enum Subcommand {
    /// Start the seeder node, detached by default.
    Start(Start),
    /// Ask the running node to shut down gracefully.
    Stop,
    /// Show node-wide status.
    Status(StatusArgs),
    /// List CIDs the node is seeding for the current repository.
    List(ListArgs),
    /// Seed an artifact whose CID is already registered in a release, and add your node's location.
    Seed(Seed),
    /// Stop seeding an artifact.
    Unseed(Unseed),
    /// Tail the node's log file.
    Logs(Logs),
}

#[derive(Parser)]
pub struct Start {
    /// Run the node attached to this terminal. Default is detached.
    #[clap(long)]
    pub foreground: bool,
    /// Reclaim the control socket if it appears stale.
    #[clap(long)]
    pub force: bool,
}

#[derive(Parser)]
pub struct StatusArgs {
    /// Emit machine-readable JSON.
    #[clap(long)]
    pub json: bool,
}

#[derive(Parser)]
pub struct ListArgs {
    /// Emit machine-readable JSON.
    #[clap(long)]
    pub json: bool,
}

#[derive(Parser)]
pub struct Seed {
    /// Path to the file or directory to seed. The CID is computed from
    /// its contents.
    pub path: std::path::PathBuf,
    /// Target release id; defaults to the most recent matching release.
    #[clap(long)]
    pub release: Option<String>,
    /// Import by reference instead of copying bytes into the store.
    #[clap(long)]
    pub reference: bool,
    /// Skip adding the radiroh://<endpoint_id> location to the COB.
    #[clap(long)]
    pub no_location: bool,
}

#[derive(Parser)]
pub struct Unseed {
    /// Content identifier of the artifact to stop seeding.
    #[clap(long)]
    pub cid: Cid,
    /// Target release id. When omitted, a CID in multiple releases
    /// prompts for one at a terminal, else sweeps every release.
    #[clap(long)]
    pub release: Option<String>,
}

#[derive(Parser)]
pub struct Logs {
    /// Follow the log; ends on Ctrl-C.
    #[clap(long, short = 'f')]
    pub follow: bool,
    /// Show only the last N lines from the existing log before
    /// streaming (when --follow) or before exiting.
    #[clap(long, short = 'n', default_value_t = 200)]
    pub lines: usize,
}

/// Errors raised by the node subcommand surface.
#[derive(Debug, Error)]
pub enum Error {
    /// Usage / argument validation error.
    #[error("{0}")]
    Usage(String),
    /// The control socket is not answering — no node is running.
    #[error("no rad-artifact node is running on this host\n  hint: start one with `rad-artifact node start`")]
    NotRunning,
    /// The detached child never bound the socket within the timeout.
    #[error("node did not come online within {0:?}; check the log file at {1}")]
    StartupTimeout(std::time::Duration, std::path::PathBuf),
    /// Parent-side lifecycle failure (passphrase, log rotation, spawn).
    #[error(transparent)]
    Lifecycle(#[from] radicle_artifact::lifecycle::LifecycleError),
    /// Generic client failure not classified as NotRunning.
    #[error("node call failed: {0}")]
    Client(#[from] radicle_artifact_client::ClientError),
    /// Local I/O error.
    #[error("I/O error")]
    Io(#[source] std::io::Error),
    /// Iroh / share / cid error from the seeder layer.
    #[error(transparent)]
    Protocol(radicle_artifact_core::Error),
    /// CID is not present in any visible release.
    #[error("artifact with CID {0} not found in any release")]
    ArtifactNotFound(Cid),
    /// Failed to write or open the release store.
    #[error("failed to write COB location for release {id}")]
    Store {
        /// Release that the write targeted.
        id: ReleaseId,
        /// Underlying cob error.
        #[source]
        err: cob::store::Error,
    },
    /// Generic release store lookup failure.
    #[error("failed to load the release store")]
    Find(#[source] cob::store::Error),
    /// JSON encoding error when emitting `--json` output.
    #[error("JSON encode failed")]
    Json(#[source] serde_json::Error),
    /// Failed to announce the COB change to the network after seed/unseed.
    #[error(transparent)]
    Announce(#[from] crate::error::Announce),
}

/// Entry point for `rad-artifact node ...`.
pub fn run(
    cli: Cli,
    repo_override: Option<RepoId>,
    no_announce: bool,
    no_input: bool,
    profile: &Profile,
) -> Result<(), Error> {
    match cli.command {
        Subcommand::Start(c) => start(c, profile),
        Subcommand::Stop => stop(profile),
        Subcommand::Status(c) => status(c, profile),
        Subcommand::List(c) => list(c, repo_override, profile),
        Subcommand::Seed(c) => seed(c, repo_override, no_announce, profile),
        Subcommand::Unseed(c) => unseed(c, repo_override, no_announce, no_input, profile),
        Subcommand::Logs(c) => logs(c, profile),
    }
}

/// Map ConnectionRefused / NotFound IO errors from the Client to the
/// friendlier "node is not running" message; pass everything else
/// through.
pub(crate) fn client_err(e: ClientError) -> Error {
    if let ClientError::Io(io) = &e {
        if matches!(
            io.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ) {
            return Error::NotRunning;
        }
    }
    Error::Client(e)
}

fn start(cmd: Start, profile: &Profile) -> Result<(), Error> {
    if cmd.foreground {
        return start_foreground(profile);
    }

    let home = profile.home.path();
    let socket = Client::default_socket(home);
    let log = lifecycle::log_path(home);

    // Cheap probe before any spawn work.
    let probe = Client::new(socket.clone());
    if probe.is_running() {
        println!("Node is already running");
        return Ok(());
    }
    if socket.exists() {
        if !cmd.force {
            return Err(Error::Usage(format!(
                "stale control socket at {}; pass --force to reclaim it",
                socket.display(),
            )));
        }
        std::fs::remove_file(&socket).map_err(Error::Io)?;
    }

    let passphrase = lifecycle::resolve_passphrase(&profile.keystore)?;
    lifecycle::rotate_log(home)?;
    let mut child = lifecycle::spawn_detached(home, passphrase.as_ref())?;

    if !lifecycle::wait_until_running(&socket, lifecycle::STARTUP_TIMEOUT) {
        // The child never answered: it may be wedged mid-bind. Kill and
        // reap it so we don't leave an orphan holding the socket/keys.
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::StartupTimeout(lifecycle::STARTUP_TIMEOUT, log));
    }

    eprintln!(
        "🌱 Node awake and seeding — listening on {}",
        socket.display()
    );
    eprintln!("   tip: check on it anytime with `rad-artifact node status`");
    Ok(())
}

/// Run the daemon attached to this terminal: spawn `rad-artifact-node`
/// with inherited stdio and wait for it to exit. The daemon logic lives
/// in that binary; this is just a convenience wrapper so the familiar
/// `node start --foreground` keeps working.
fn start_foreground(profile: &Profile) -> Result<(), Error> {
    let exe = lifecycle::find_node_bin()?;
    let passphrase = lifecycle::resolve_passphrase(&profile.keystore)?;

    let mut cmd = std::process::Command::new(exe);
    if let Some(p) = passphrase.as_ref() {
        cmd.env(lifecycle::PASSPHRASE_ENV, p.as_str());
    }
    let status = cmd.status().map_err(Error::Io)?;
    if !status.success() {
        return Err(Error::Usage(format!(
            "{} exited: {status}",
            lifecycle::NODE_BIN
        )));
    }
    Ok(())
}

fn stop(profile: &Profile) -> Result<(), Error> {
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    client
        .call::<()>(&NodeMsg::Shutdown, client::DEFAULT_TIMEOUT)
        .map_err(client_err)?;
    eprintln!("💤 Node stopped — the seeds rest");
    Ok(())
}

fn status(cmd: StatusArgs, profile: &Profile) -> Result<(), Error> {
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    let status = client
        .call::<Status>(&NodeMsg::Status, client::DEFAULT_TIMEOUT)
        .map_err(client_err)?;
    if cmd.json {
        let s = serde_json::to_string_pretty(&status).map_err(Error::Json)?;
        println!("{s}");
    } else {
        print_status_pretty(&status);
    }
    Ok(())
}

fn list(cmd: ListArgs, repo_override: Option<RepoId>, profile: &Profile) -> Result<(), Error> {
    let repo = open_repo(repo_override, profile).map_err(|e| Error::Usage(e.to_string()))?;
    let rid = repo.id;
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    let entries = client
        .call::<Vec<SeededEntry>>(&NodeMsg::ListSeeded { rid }, client::DEFAULT_TIMEOUT)
        .map_err(client_err)?;
    if cmd.json {
        let s = serde_json::to_string_pretty(&entries).map_err(Error::Json)?;
        println!("{s}");
        return Ok(());
    }
    if entries.is_empty() {
        println!("🌾 Nothing seeded for {rid} yet.");
        return Ok(());
    }
    println!("🌱 Seeding {} artifact(s) for {rid}:", entries.len());
    for entry in &entries {
        println!("  {}", seeded_line(entry));
    }
    Ok(())
}

/// Render one `node list` row.
///
/// A tag only records the intent to seed, so a row whose bytes the store
/// can't back up is annotated.
fn seeded_line(entry: &SeededEntry) -> String {
    match entry.complete {
        Some(true) => format!("{} ({})", entry.cid, human_bytes(entry.bytes)),
        Some(false) => format!(
            "{} ⚠ incomplete — only {} in the store",
            entry.cid,
            human_bytes(entry.bytes)
        ),
        // Unknown, not missing: an older node omits the field, and a
        // momentary store error reads the same way.
        None => format!(
            "{} ({}) ⚠ completeness unknown",
            entry.cid,
            human_bytes(entry.bytes)
        ),
    }
}

fn seed(
    cmd: Seed,
    repo_override: Option<RepoId>,
    no_announce: bool,
    profile: &Profile,
) -> Result<(), Error> {
    seed_artifact(
        cmd.path,
        cmd.release,
        cmd.reference,
        cmd.no_location,
        no_announce,
        repo_override,
        profile,
    )
}

/// Shared implementation for `rad-artifact node seed <PATH>` and its
/// top-level alias `rad-artifact seed <PATH>`.
///
/// Computes the CID from `<PATH>`, sends the seed request to the
/// running node, and (unless `no_location`) writes the
/// `radiroh://{endpoint_id}` location to the target release.
pub(crate) fn seed_artifact(
    path: std::path::PathBuf,
    release_override: Option<String>,
    reference: bool,
    no_location: bool,
    no_announce: bool,
    repo_override: Option<RepoId>,
    profile: &Profile,
) -> Result<(), Error> {
    // Same codec split as `rad-artifact add <PATH>`: raw for files,
    // blake3-hashseq for directories.
    let cid = if path.is_dir() {
        share::compute_content_id(&path).map_err(Error::Io)?
    } else {
        share::compute_blob_cid(&path).map_err(Error::Protocol)?
    };
    let repo = open_repo(repo_override, profile).map_err(|e| Error::Usage(e.to_string()))?;
    let mut releases = open_releases(&repo, profile).map_err(|e| Error::Usage(e.to_string()))?;
    let rid = repo.id;

    // Resolve target release: --release wins, else most recent matching.
    let release_id = if let Some(s) = release_override.as_deref() {
        parse_release_id(s, &repo).map_err(|e| Error::Usage(e.to_string()))?
    } else {
        let matching = releases.find_by_cid(&cid).map_err(Error::Find)?;
        let (id, _) = matching
            .into_iter()
            .max_by_key(|(_, r)| r.timestamp())
            .ok_or(Error::ArtifactNotFound(cid))?;
        id
    };

    seed_to_release(
        &path,
        cid,
        release_id,
        reference,
        no_location,
        rid,
        &mut releases,
        profile,
    )?;

    // Announce the new location so peers can discover it. Without this the
    // COB write stays local until something else announces. `--no-announce`
    // defers this; `rad sync -a` sends it later. Skip when nothing was
    // written to the COB (`--no-location`).
    if !no_location && !no_announce {
        crate::announce(profile, rid)?;
    }
    Ok(())
}

/// Hand an artifact's bytes to the running node and, unless `no_location`,
/// sign its `radiroh://` location into `release_id`.
///
/// `cid` must already be the CID of `path`'s contents: the caller computed
/// it, so this does not hash the file again — this is what lets `register
/// --seed` register and seed in a single pass. Does not announce; the
/// caller announces once every COB write for the run is done.
#[allow(clippy::too_many_arguments)]
pub(crate) fn seed_to_release(
    path: &std::path::Path,
    cid: Cid,
    release_id: ReleaseId,
    reference: bool,
    no_location: bool,
    rid: RepoId,
    releases: &mut Releases<'_, Repository>,
    profile: &Profile,
) -> Result<(), Error> {
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    let kind = share::artifact_kind(&cid).map_err(Error::Protocol)?;
    let mode = if reference {
        ImportMode::Reference
    } else {
        ImportMode::Copy
    };

    // Canonicalise so the node, which runs from its own cwd, resolves
    // the same file the user pointed at.
    let abs_path = path
        .canonicalize()
        .map_err(|e| Error::Usage(format!("cannot resolve path {}: {e}", path.display())))?;

    // Generous timeout: collection imports can take a while.
    let receipt = client
        .call::<SeedReceipt>(
            &NodeMsg::Seed {
                rid,
                release: release_id.oid(),
                cid,
                path: abs_path,
                kind,
                mode,
            },
            Duration::from_secs(300),
        )
        .map_err(client_err)?;

    let new_or_dup = if receipt.was_new { "new" } else { "already" };
    eprintln!(
        "🌱 Seeded {} ({}, {new_or_dup} tagged)",
        cid,
        human_bytes(receipt.bytes)
    );

    if no_location {
        eprintln!("🤫 Keeping it local — no COB location written (--no-location)");
        return Ok(());
    }

    // The node has already tagged the artifact. If adding the COB
    // location now fails, leave the tag in place and tell the user how
    // to retry
    if let Err(e) = add_location(releases, release_id, cid, &receipt, profile) {
        eprintln!("⚠️  Seed tagged, but adding the artifact location failed: {e}");
        eprintln!("   Retry with: rad-artifact seed {}", path.display());
        return Err(e);
    }
    eprintln!("📡 Added a radiroh:// location in release {release_id}");
    Ok(())
}

/// Sign the seed's `radiroh://` location into the target release's COB.
fn add_location(
    releases: &mut Releases<'_, Repository>,
    release_id: ReleaseId,
    cid: Cid,
    receipt: &SeedReceipt,
    profile: &Profile,
) -> Result<(), Error> {
    let signer = profile
        .signer()
        .map_err(|e| Error::Usage(format!("signer: {e}")))?;
    // Receipt's endpoint_id is already a validated EndpointId; the
    // protocol decode would have rejected a malformed value.
    let url = receipt.endpoint_id.to_url();
    let mut release_mut = releases.get_mut(&release_id).map_err(Error::Find)?;
    release_mut
        .add_location(cid, url, &signer)
        .map_err(|err| Error::Store {
            id: release_id,
            err,
        })?;
    Ok(())
}

fn unseed(
    cmd: Unseed,
    repo_override: Option<RepoId>,
    no_announce: bool,
    no_input: bool,
    profile: &Profile,
) -> Result<(), Error> {
    unseed_artifact(
        cmd.cid,
        cmd.release,
        no_announce,
        no_input,
        repo_override,
        profile,
    )
}

/// Shared implementation for `rad-artifact unseed --cid <CID>` and
/// `rad-artifact node unseed --cid <CID>`.
///
/// Sends the unseed request to the running node and retracts every
/// `radiroh://` location under our DID for the given CID. `release_override`
/// restricts the retraction to a single release id. With no override, a CID
/// shared by several releases prompts the user to pick one (or "All") at a
/// terminal; otherwise every release containing the CID is swept.
pub(crate) fn unseed_artifact(
    cid: Cid,
    release_override: Option<String>,
    no_announce: bool,
    no_input: bool,
    repo_override: Option<RepoId>,
    profile: &Profile,
) -> Result<(), Error> {
    let repo = open_repo(repo_override, profile).map_err(|e| Error::Usage(e.to_string()))?;
    let mut releases = open_releases(&repo, profile).map_err(|e| Error::Usage(e.to_string()))?;
    let rid = repo.id;
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);

    // `--release` scopes both the tag removal and the COB retraction to one
    // release. Without it, a CID in more than one release is ambiguous: at a
    // terminal we let the user scope it (or pick "All releases"); otherwise
    // every release containing the CID is swept.
    let release_filter: Option<ReleaseId> = match release_override.as_deref() {
        Some(s) => Some(parse_release_id(s, &repo).map_err(|e| Error::Usage(e.to_string()))?),
        None => {
            let candidates = releases.find_by_cid(&cid).map_err(Error::Find)?;
            if candidates.len() > 1 && !no_input && std::io::stdin().is_terminal() {
                match crate::prompt::pick_unseed_release(&candidates, &repo, profile)
                    .map_err(Error::Usage)?
                {
                    crate::prompt::UnseedScope::One(id) => Some(id),
                    crate::prompt::UnseedScope::All => None,
                }
            } else {
                None
            }
        }
    };

    let receipt = client
        .call::<UnseedReceipt>(
            &NodeMsg::Unseed {
                rid,
                release: release_filter.map(|r| r.oid()),
                cid,
            },
            client::DEFAULT_TIMEOUT,
        )
        .map_err(client_err)?;

    if receipt.was_removed {
        eprintln!("🍂 Unseeded {cid} — no longer holding its bytes");
    } else {
        eprintln!("🤷 Nothing to do — {cid} wasn't being seeded");
    }

    let local_did = Did::from(*profile.id());
    let signer = profile
        .signer()
        .map_err(|e| Error::Usage(format!("signer: {e}")))?;

    let target_ids: Vec<ReleaseId> = if let Some(id) = release_filter {
        vec![id]
    } else {
        releases
            .find_by_cid(&cid)
            .map_err(Error::Find)?
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    };

    let mut removed = 0u32;
    for id in target_ids {
        let mut release_mut = match releases.get_mut(&id) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Warning: cannot open release {id} for unseed: {e}");
                continue;
            }
        };
        // Snapshot the URLs we want to remove before mutating the COB,
        // so the borrow on release_mut doesn't overlap the writes.
        let urls_to_remove: Vec<Url> = release_mut
            .artifact(&cid)
            .and_then(|a| a.locations_of(&local_did))
            .map(|urls| {
                urls.iter()
                    .filter(|u| EndpointId::is_endpoint_url(u))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for url in urls_to_remove {
            match release_mut.remove_location(cid, url.clone(), &signer) {
                Ok(_) => removed += 1,
                Err(e) => eprintln!("Warning: failed to remove {url} from {id}: {e}"),
            }
        }
    }
    if removed > 0 {
        eprintln!("🧹 Swept {removed} iroh location(s) from the COB");
        // Announce the retractions so peers stop advertising us as a source.
        // `--no-announce` defers this to a later `rad sync -a`.
        if !no_announce {
            crate::announce(profile, rid)?;
        }
    }
    Ok(())
}

fn logs(cmd: Logs, profile: &Profile) -> Result<(), Error> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let path = lifecycle::log_path(profile.home.path());
    if !path.exists() {
        eprintln!("No log file at {}", path.display());
        return Ok(());
    }

    // Print the last N lines of the existing log up front.
    let content = std::fs::read_to_string(&path).map_err(Error::Io)?;
    let total = content.lines().count();
    let skip = total.saturating_sub(cmd.lines);
    for line in content.lines().skip(skip) {
        println!("{line}");
    }

    if !cmd.follow {
        return Ok(());
    }

    // Follow new bytes from EOF. Ctrl-C kills the process — fine.
    let file = std::fs::File::open(&path).map_err(Error::Io)?;
    let mut reader = BufReader::new(file);
    let end = std::fs::metadata(&path).map_err(Error::Io)?.len();
    reader.seek(SeekFrom::Start(end)).map_err(Error::Io)?;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(Error::Io)?;
        if n == 0 {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        print!("{line}");
    }
}

fn print_status_pretty(s: &Status) {
    // Print the full radiroh:// URL — peers need to copy it verbatim, so
    // truncation here is hostile.
    let uptime = humanize_uptime(s.started_at_unix);
    println!("Node          {} (started {uptime})", s.endpoint_id);
    println!(
        "Seeded        {} artifacts · {}",
        s.seeded.count,
        human_bytes(s.seeded.bytes_logical)
    );
    let conn = &s.connections;
    println!(
        "Connections   {} active · {} opened · {} closed · {} direct · {} holepunches",
        conn.active,
        conn.opened_total,
        conn.closed_total,
        conn.direct_total,
        conn.holepunch_attempts
    );
    println!(
        "Paths         {} direct · {} relayed",
        conn.paths_direct, conn.paths_relayed
    );
    let tr = &s.traffic;
    println!(
        "Traffic       {} out · {} in",
        human_bytes(tr.out_bytes),
        human_bytes(tr.in_bytes)
    );
    print_relay(&s.relay);
}

fn print_relay(r: &RelayStats) {
    if r.relays.is_empty() {
        println!("Relay         none assigned — node may be unreachable");
        return;
    }
    for relay in &r.relays {
        let state = if relay.connected {
            "connected"
        } else {
            "DISCONNECTED"
        };
        let latency = match relay.latency_ms {
            Some(ms) => format!(" · {ms}ms"),
            None => String::new(),
        };
        // Show the last error only when it adds signal (disconnected).
        let err = match (&relay.last_error, relay.connected) {
            (Some(e), false) => format!(" · {e}"),
            _ => String::new(),
        };
        println!("Relay         {state} · {}{latency}{err}", relay.url);
    }
    // UDP reachability decides whether peers can holepunch a direct path.
    println!(
        "Reachability  UDP {} · {}",
        if r.udp_v4 { "v4" } else { "no-v4" },
        if r.udp_v6 { "v6" } else { "no-v6" },
    );
}

fn humanize_uptime(started_at_unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - started_at_unix).max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h {}m ago", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid() -> Cid {
        share::blake3_hash_to_cid(
            blake3::hash(b"seeded-line-sample"),
            share::ArtifactKind::Blob,
        )
    }

    #[test]
    fn complete_row_is_unadorned() {
        let entry = SeededEntry {
            cid: cid(),
            bytes: 2048,
            complete: Some(true),
        };
        assert_eq!(seeded_line(&entry), format!("{} (2.0 KiB)", cid()));
    }

    #[test]
    fn incomplete_row_reports_what_is_there() {
        let entry = SeededEntry {
            cid: cid(),
            bytes: 1024,
            complete: Some(false),
        };
        assert_eq!(
            seeded_line(&entry),
            format!("{} ⚠ incomplete — only 1.0 KiB in the store", cid())
        );
    }

    /// A node that predates the field, or a store hiccup, must not read as
    /// "bytes missing".
    #[test]
    fn unknown_row_says_unknown() {
        let entry = SeededEntry {
            cid: cid(),
            bytes: 1024,
            complete: None,
        };
        assert_eq!(
            seeded_line(&entry),
            format!("{} (1.0 KiB) ⚠ completeness unknown", cid())
        );
    }
}
