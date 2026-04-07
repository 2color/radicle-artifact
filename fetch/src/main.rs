//! Fetch artifacts from Radicle Artifact COBs.
//!
//! Run `rad-fetch --help` for usage.

use std::error::Error as _;
use std::ops::Deref;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use clap::Parser;

use radicle::cob;
use radicle::git::Oid;
use radicle::identity::Did;
use radicle::prelude::{Profile, ReadStorage, RepoId};
use radicle::storage::git::Repository;
use radicle_artifact::*;
use radicle_artifact_fetch::{
    ArtifactKind, Location, artifact_kind, default_fetchers, download, download_collection,
};

/// Fetch an artifact from a Radicle release COB.
///
/// With positional arguments, fetches a specific artifact directly.
/// Without arguments, interactively lists releases and artifacts to pick from.
#[derive(Parser)]
#[clap(version)]
struct Args {
    /// Use this repository. Default is the current working directory.
    #[clap(short, long)]
    repository: Option<RepoId>,

    /// Git object ID of the release.
    oid: Option<Oid>,

    /// Content identifier (CID) of the artifact to fetch.
    cid: Option<Cid>,

    /// Output file path. Defaults to the artifact name in the current directory.
    #[clap(short, long)]
    output: Option<PathBuf>,

    /// Fetch from this URL directly, skipping registered locations.
    #[clap(long)]
    url: Option<url::Url>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("ERROR: {err}");
        let mut err = err.source();
        while let Some(underlying) = err {
            eprintln!("caused by: {underlying}");
            err = underlying.source();
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), RadFetchError> {
    let args = Args::parse();
    let profile = Profile::load().map_err(RadFetchError::Profile)?;
    let repo = open_repo(&args, &profile)?;
    let releases = Releases::open(&repo).map_err(|err| RadFetchError::Releases {
        rid: repo.id,
        err,
    })?;

    let (oid, cid) = match (args.oid, args.cid) {
        (Some(oid), Some(cid)) => (oid, cid),
        (None, None) => pick_interactive(&releases)?,
        _ => {
            return Err(RadFetchError::Usage(
                "provide both <oid> and <cid>, or neither for interactive mode".into(),
            ))
        }
    };

    // Find the release and artifact.
    let release_id = find_unique_by_oid(oid, &releases)?;
    let release = releases
        .get(&release_id)
        .map_err(|err| RadFetchError::Find(FindError::Lookup { oid, err }))?
        .ok_or(RadFetchError::Find(FindError::NoRelease(oid)))?;

    let artifact = release
        .artifact(&cid)
        .ok_or(RadFetchError::ArtifactNotFound(cid))?;

    if artifact.is_redacted() {
        eprintln!("WARNING: this artifact has been redacted");
        for (did, reason) in artifact.redactions() {
            eprintln!("  {did}: {reason}");
        }
    }

    // Collect locations to try.
    let locations = if let Some(ref url) = args.url {
        vec![Location::Url(url)]
    } else {
        artifact_locations(artifact)?
    };

    // Download, branching on whether this is a single blob or a collection.
    let output_path = args
        .output
        .unwrap_or_else(|| PathBuf::from(artifact.name()));

    match artifact_kind(&cid).map_err(RadFetchError::Fetch)? {
        ArtifactKind::Blob => {
            let mut file = File::create(&output_path).map_err(RadFetchError::Io)?;
            let fetchers = default_fetchers();
            download(&locations, &cid, &mut file, &fetchers).map_err(RadFetchError::Fetch)?;
        }
        ArtifactKind::Collection => {
            download_collection(&locations, &cid, &output_path).map_err(RadFetchError::Fetch)?;
        }
    }

    println!("{}", output_path.display());
    Ok(())
}

/// Convert artifact locations into fetch locations.
///
/// For `radworks://` URLs, derives the iroh endpoint ID from the contributing
/// DID's Ed25519 public key. For all other URLs, passes them through as-is.
fn artifact_locations(artifact: &Artifact) -> Result<Vec<Location<'_>>, RadFetchError> {
    let mut locations = Vec::new();
    for (did, urls) in artifact.locations() {
        for url in urls {
            if url.scheme() == "radworks" {
                let endpoint_id = did_to_endpoint_id(did)?;
                locations.push(Location::Iroh(endpoint_id));
            } else {
                locations.push(Location::Url(url));
            }
        }
    }
    Ok(locations)
}

/// Derive an iroh endpoint ID from a Radicle DID.
///
/// Both use Ed25519 public keys, so this is a direct byte-level conversion.
fn did_to_endpoint_id(did: &Did) -> Result<iroh::EndpointId, RadFetchError> {
    let bytes: &[u8; 32] = did.as_key().deref();
    iroh::PublicKey::from_bytes(bytes).map_err(|e| {
        RadFetchError::Usage(format!("invalid Ed25519 key in DID {did}: {e}"))
    })
}

fn open_repo(args: &Args, profile: &Profile) -> Result<Repository, RadFetchError> {
    let repo_id = if let Some(repo_id) = args.repository {
        repo_id
    } else {
        let (_repo, repo_id) =
            radicle::rad::cwd().map_err(|e| RadFetchError::Repository(e.to_string()))?;
        repo_id
    };
    profile
        .storage
        .repository(repo_id)
        .map_err(|e| RadFetchError::Repository(e.to_string()))
}

fn find_unique_by_oid(
    oid: Oid,
    releases: &Releases<Repository>,
) -> Result<ReleaseId, RadFetchError> {
    let mut iter = releases
        .find_by_oid(oid)
        .map_err(|err| RadFetchError::Find(FindError::Lookup { oid, err }))?;
    let (id, _release) = iter
        .next()
        .ok_or(RadFetchError::Find(FindError::NoRelease(oid)))?
        .map_err(|err| RadFetchError::Find(FindError::Lookup { oid, err }))?;

    if iter.next().is_some() {
        return Err(RadFetchError::Find(FindError::Ambiguous(oid)));
    }
    Ok(id)
}

/// Interactive mode: list releases, pick one, list its artifacts, pick one.
fn pick_interactive(
    releases: &Releases<Repository>,
) -> Result<(Oid, Cid), RadFetchError> {
    let all: Vec<(ReleaseId, Release)> = releases
        .all()
        .map_err(|e| RadFetchError::Repository(e.to_string()))?
        .filter_map(|res| res.ok())
        .map(|(id, release)| (ReleaseId::from(id), release))
        .collect();

    if all.is_empty() {
        return Err(RadFetchError::Usage("no releases found in this repository".into()));
    }

    // Step 1: pick a release.
    eprintln!("Releases:");
    for (i, (_, release)) in all.iter().enumerate() {
        let artifact_count = release.artifacts().len();
        eprintln!(
            "  [{}] {} ({} artifact{})",
            i + 1,
            release.oid(),
            artifact_count,
            if artifact_count == 1 { "" } else { "s" }
        );
    }

    let release_idx = prompt_choice("Select release", all.len())?;
    let (_, release) = &all[release_idx];

    // Step 2: pick an artifact.
    let artifacts: Vec<(&Cid, &Artifact)> = release.artifacts().iter().collect();
    if artifacts.is_empty() {
        return Err(RadFetchError::Usage(
            "selected release has no artifacts".into(),
        ));
    }

    eprintln!("Artifacts:");
    for (i, (cid, artifact)) in artifacts.iter().enumerate() {
        let redacted = if artifact.is_redacted() {
            " [REDACTED]"
        } else {
            ""
        };
        eprintln!("  [{}] {} (CID: {}){}", i + 1, artifact.name(), cid, redacted);
    }

    let artifact_idx = prompt_choice("Select artifact", artifacts.len())?;
    let (cid, _) = artifacts[artifact_idx];

    Ok((*release.oid(), *cid))
}

/// Prompt user for a 1-indexed choice, return 0-indexed.
fn prompt_choice(label: &str, max: usize) -> Result<usize, RadFetchError> {
    let stdin = io::stdin();
    loop {
        eprint!("{label} [1-{max}]: ");
        io::stderr().flush().map_err(RadFetchError::Io)?;

        let mut line = String::new();
        stdin.lock().read_line(&mut line).map_err(RadFetchError::Io)?;

        match line.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= max => return Ok(n - 1),
            _ => eprintln!("Invalid choice, try again."),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum RadFetchError {
    #[error("failed to load Radicle profile")]
    Profile(#[source] radicle::profile::Error),

    #[error("{0}")]
    Repository(String),

    #[error("failed to open release store for {rid}")]
    Releases {
        rid: RepoId,
        #[source]
        err: radicle::storage::RepositoryError,
    },

    #[error(transparent)]
    Find(#[from] FindError),

    #[error("artifact with CID {0} not found in release")]
    ArtifactNotFound(Cid),

    #[error("{0}")]
    Usage(String),

    #[error(transparent)]
    Fetch(radicle_artifact_fetch::FetchError),

    #[error("I/O error")]
    Io(#[source] io::Error),
}

#[derive(Debug, thiserror::Error)]
enum FindError {
    #[error("no release found for commit {0}")]
    NoRelease(Oid),

    #[error("multiple releases found for commit {0}, use a release ID to disambiguate")]
    Ambiguous(Oid),

    #[error("failed to find release for commit {oid}")]
    Lookup {
        oid: Oid,
        #[source]
        err: cob::store::Error,
    },
}
