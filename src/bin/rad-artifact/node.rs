//! `rad-artifact node` subcommand: control the local seeder daemon.
//!
//! Talks to the long-running [`radicle_artifact::node`] over the Unix
//! control socket. Subcommands that need a repository (list, seed,
//! unseed) honor the top-level `--repository <RID>` override or fall
//! back to the cwd's radicle repo. Subcommands that don't (start, stop,
//! status, logs) skip repo discovery entirely so they work from any
//! directory.
//!
//! Pretty-print helpers (`print_status_pretty`, `human_bytes`,
//! `humanize_uptime`) live here too — they're node-specific UI.

use std::time::Duration;

use clap::Parser;
use radicle::cob;
use radicle::{
    identity::{Did, RepoId},
    prelude::Profile,
};
use radicle_artifact::client::{self, Client, ClientError};
use radicle_artifact::node;
use radicle_artifact::protocol::{
    Command as NodeMsg, ImportMode, SeedReceipt, SeededEntry, Status, UnseedReceipt,
};
use radicle_artifact::share;
use radicle_artifact::{Cid, ReleaseId};
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
    /// Tell the node to seed an artifact already published in a release.
    Seed(Seed),
    /// Tell the node to stop seeding an artifact.
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
    /// Content identifier of the artifact to seed.
    pub cid: Cid,
    /// Path to the artifact on disk.
    pub path: std::path::PathBuf,
    /// Target release id; defaults to the most recent matching release.
    #[clap(long)]
    pub release: Option<String>,
    /// Import by reference instead of copying bytes into the store.
    #[clap(long)]
    pub reference: bool,
    /// Skip writing the iroh:// location to the COB.
    #[clap(long)]
    pub no_announce: bool,
}

#[derive(Parser)]
pub struct Unseed {
    /// Content identifier of the artifact to stop seeding.
    pub cid: Cid,
    /// Target release id; defaults to every matching release.
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
    Lifecycle(#[from] radicle_artifact::node::lifecycle::LifecycleError),
    /// Foreground node returned an error from its accept loop.
    #[error(transparent)]
    Run(#[from] radicle_artifact::node::NodeError),
    /// Generic client failure not classified as NotRunning.
    #[error("node call failed: {0}")]
    Client(#[from] radicle_artifact::client::ClientError),
    /// Local I/O error.
    #[error("I/O error")]
    Io(#[source] std::io::Error),
    /// Iroh / share / cid error from the seeder layer.
    #[error(transparent)]
    Protocol(radicle_artifact::share::Error),
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
}

/// Entry point for `rad-artifact node ...`.
pub fn run(cli: Cli, repo_override: Option<RepoId>, profile: &Profile) -> Result<(), Error> {
    match cli.command {
        Subcommand::Start(c) => start(c, profile),
        Subcommand::Stop => stop(profile),
        Subcommand::Status(c) => status(c, profile),
        Subcommand::List(c) => list(c, repo_override, profile),
        Subcommand::Seed(c) => seed(c, repo_override, profile),
        Subcommand::Unseed(c) => unseed(c, repo_override, profile),
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
    let log = node::lifecycle::log_path(home);

    // Cheap probe before any spawn work.
    let probe = Client::new(socket.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Io)?;
    if rt.block_on(probe.is_running()) {
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

    let passphrase = node::lifecycle::resolve_passphrase(&profile.keystore)?;
    node::lifecycle::rotate_log(home)?;
    node::lifecycle::spawn_detached(home, passphrase.as_ref(), cmd.force)?;

    if !node::lifecycle::wait_until_running(&socket, node::lifecycle::STARTUP_TIMEOUT) {
        return Err(Error::StartupTimeout(node::lifecycle::STARTUP_TIMEOUT, log));
    }

    eprintln!("Node started (socket: {})", socket.display());
    Ok(())
}

fn start_foreground(profile: &Profile) -> Result<(), Error> {
    // JSON-format tracing to stderr, which the detached parent
    // redirects into node.log. Iroh uses tracing too, so this
    // subscriber catches both our events and iroh's, gated by
    // RUST_LOG. Default keeps iroh quiet and our own crate at info.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("warn,iroh=warn,iroh_blobs=warn,radicle_artifact=info")
    });
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();

    let passphrase = node::lifecycle::resolve_passphrase(&profile.keystore)?;
    let secret =
        share::radicle_secret_to_iroh(&profile.keystore, passphrase).map_err(Error::Protocol)?;
    let home = profile.home.path().to_path_buf();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Error::Io)?;
    rt.block_on(node::run(&home, secret))?;
    Ok(())
}

fn stop(profile: &Profile) -> Result<(), Error> {
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    client
        .call_blocking::<()>(&NodeMsg::Shutdown, client::DEFAULT_TIMEOUT)
        .map_err(client_err)?;
    eprintln!("Node stopped");
    Ok(())
}

fn status(cmd: StatusArgs, profile: &Profile) -> Result<(), Error> {
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    let status = client
        .call_blocking::<Status>(&NodeMsg::Status, client::DEFAULT_TIMEOUT)
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
    let rid = repo.id.to_string();
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);
    let entries = client
        .call_blocking::<Vec<SeededEntry>>(
            &NodeMsg::ListSeeded { rid: rid.clone() },
            client::DEFAULT_TIMEOUT,
        )
        .map_err(client_err)?;
    if cmd.json {
        let s = serde_json::to_string_pretty(&entries).map_err(Error::Json)?;
        println!("{s}");
        return Ok(());
    }
    if entries.is_empty() {
        println!("No artifacts are being seeded for {rid}.");
        return Ok(());
    }
    println!("Seeding {} artifact(s) for {rid}:", entries.len());
    for entry in entries {
        println!("  {} ({})", entry.cid, human_bytes(entry.bytes));
    }
    Ok(())
}

fn seed(cmd: Seed, repo_override: Option<RepoId>, profile: &Profile) -> Result<(), Error> {
    seed_artifact(
        cmd.cid,
        cmd.path,
        cmd.release,
        cmd.reference,
        cmd.no_announce,
        repo_override,
        profile,
    )
}

/// Shared implementation for `rad-artifact seed <PATH>` (the top-level
/// renamed `serve`) and `rad-artifact node seed <CID> <PATH>`.
///
/// Sends the seed request to the running node and, unless
/// `no_announce`, writes the `iroh://{endpoint_id}` location to the
/// target release.
pub(crate) fn seed_artifact(
    cid: Cid,
    path: std::path::PathBuf,
    release_override: Option<String>,
    reference: bool,
    no_announce: bool,
    repo_override: Option<RepoId>,
    profile: &Profile,
) -> Result<(), Error> {
    let repo = open_repo(repo_override, profile).map_err(|e| Error::Usage(e.to_string()))?;
    let mut releases = open_releases(&repo).map_err(|e| Error::Usage(e.to_string()))?;
    let rid = repo.id.to_string();
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);

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
        .call_blocking::<SeedReceipt>(
            &NodeMsg::Seed {
                rid,
                cid: cid.to_string(),
                path: abs_path,
                kind,
                mode,
            },
            Duration::from_secs(300),
        )
        .map_err(client_err)?;

    let new_or_dup = if receipt.was_new { "new" } else { "already" };
    eprintln!(
        "Seeded {} ({}, {new_or_dup} tagged)",
        cid,
        human_bytes(receipt.bytes)
    );

    if no_announce {
        eprintln!("Skipped COB location write (--no-announce)");
        return Ok(());
    }

    let signer = profile
        .signer()
        .map_err(|e| Error::Usage(format!("signer: {e}")))?;
    let url = share::iroh_url::build_from_id_str(&receipt.endpoint_id)
        .map_err(|e| Error::Usage(format!("invalid endpoint id from node: {e}")))?;
    let mut release_mut = releases.get_mut(&release_id).map_err(Error::Find)?;
    release_mut
        .add_location(cid, url, &signer)
        .map_err(|err| Error::Store {
            id: release_id,
            err,
        })?;
    eprintln!("Added iroh location to release {release_id}");
    Ok(())
}

fn unseed(cmd: Unseed, repo_override: Option<RepoId>, profile: &Profile) -> Result<(), Error> {
    unseed_artifact(cmd.cid, cmd.release, repo_override, profile)
}

/// Shared implementation for `rad-artifact unseed <CID>` and
/// `rad-artifact node unseed <CID>`.
///
/// Sends the unseed request to the running node and retracts every
/// `iroh://` location under our DID for the given CID. `release_override`
/// restricts the retraction to a single release id; otherwise every
/// release containing the CID is scanned.
pub(crate) fn unseed_artifact(
    cid: Cid,
    release_override: Option<String>,
    repo_override: Option<RepoId>,
    profile: &Profile,
) -> Result<(), Error> {
    let repo = open_repo(repo_override, profile).map_err(|e| Error::Usage(e.to_string()))?;
    let mut releases = open_releases(&repo).map_err(|e| Error::Usage(e.to_string()))?;
    let rid = repo.id.to_string();
    let socket = Client::default_socket(profile.home.path());
    let client = Client::new(socket);

    let receipt = client
        .call_blocking::<UnseedReceipt>(
            &NodeMsg::Unseed {
                rid,
                cid: cid.to_string(),
            },
            client::DEFAULT_TIMEOUT,
        )
        .map_err(client_err)?;

    if receipt.was_removed {
        eprintln!("Unseeded {cid}");
    } else {
        eprintln!("Note: {cid} was not being seeded");
    }

    let local_did = Did::from(*profile.id());
    let signer = profile
        .signer()
        .map_err(|e| Error::Usage(format!("signer: {e}")))?;

    let target_ids: Vec<ReleaseId> = if let Some(s) = release_override.as_deref() {
        vec![parse_release_id(s, &repo).map_err(|e| Error::Usage(e.to_string()))?]
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
                    .filter(|u| share::iroh_url::matches(u))
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
        eprintln!("Removed {removed} iroh location(s) from COB");
    }
    Ok(())
}

fn logs(cmd: Logs, profile: &Profile) -> Result<(), Error> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let path = node::lifecycle::log_path(profile.home.path());
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
    // Print the full base32 endpoint id — peers need to copy it
    // verbatim, so truncation here is hostile.
    let uptime = humanize_uptime(s.started_at_unix);
    println!("Node          {} (started {uptime})", s.endpoint_id);
    println!(
        "Seeded        {} artifacts · {}",
        s.seeded.count,
        human_bytes(s.seeded.bytes_logical)
    );
    println!(
        "Disk          {} on disk · {} logical",
        human_bytes(s.disk.store_bytes),
        human_bytes(s.disk.seeded_bytes_logical)
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
    if s.warnings.did_locations_unmatched > 0 {
        println!(
            "Warnings      ⚠ {} stale endpoint URL(s) under your DID (run `rad-artifact reconcile --retract-orphaned-self`)",
            s.warnings.did_locations_unmatched
        );
    } else {
        println!("Warnings      none");
    }
}

fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
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
