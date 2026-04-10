//! Share artifacts from Radicle Artifact COBs.
//!
//! Run `rad-share --help` for usage.

use std::error::Error as _;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use clap::Parser;

use radicle::cob;
use radicle::git::Oid;
use radicle::prelude::{Profile, ReadStorage, RepoId};
use radicle::storage::git::Repository;
use radicle_artifact::*;
use radicle_artifact_share::{
    ArtifactKind, EndpointPreset, Location, Server, add_blob, add_collection, artifact_kind,
    default_fetchers, download, download_collection,
};

#[derive(Parser)]
#[clap(version)]
enum Cli {
    /// Compute the BLAKE3 CID of a file or directory
    Cid(CidArgs),
    /// Fetch an artifact from a release COB
    Fetch(FetchArgs),
    /// Serve an artifact via iroh-blobs using your radicle identity
    Serve(ServeArgs),
}

/// Compute the BLAKE3 CID of a file or directory.
///
/// For a single file, outputs a CID with the raw codec (0x55).
/// For a directory, outputs a CID with the blake3-hashseq codec (0x80).
#[derive(clap::Args)]
struct CidArgs {
    /// Path to file or directory.
    path: PathBuf,
}

/// Fetch an artifact from a Radicle release COB.
///
/// With positional arguments, fetches a specific artifact directly.
/// Without arguments, interactively lists releases and artifacts to pick from.
#[derive(clap::Args)]
struct FetchArgs {
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

/// Serve an artifact via iroh-blobs using your radicle identity.
///
/// Verifies the file/directory matches the artifact CID, registers an
/// `iroh://<endpoint_id>` location in the release COB, and serves the
/// content until interrupted.
#[derive(clap::Args)]
struct ServeArgs {
    /// Path to file or directory to serve.
    path: PathBuf,

    /// Use this repository. Default is the current working directory.
    #[clap(short, long)]
    repository: Option<RepoId>,

    /// Artifact CID. If omitted, launches interactive picker.
    cid: Option<Cid>,
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

fn run() -> Result<(), RadShareError> {
    match Cli::parse() {
        Cli::Cid(args) => run_cid(args),
        Cli::Fetch(args) => run_fetch(args),
        Cli::Serve(args) => run_serve(args),
    }
}

fn run_cid(args: CidArgs) -> Result<(), RadShareError> {
    let path = &args.path;
    if path.is_dir() {
        // Build a collection: hash each file, then build the hashseq.
        let rt = tokio::runtime::Runtime::new().map_err(RadShareError::Io)?;
        let cid = rt.block_on(async {
            let mem_store = iroh_blobs::store::mem::MemStore::new();
            let store: iroh_blobs::api::Store = mem_store.into();
            let mut entries = Vec::new();

            let mut stack = vec![path.to_path_buf()];
            while let Some(current) = stack.pop() {
                let read_dir = std::fs::read_dir(&current).map_err(RadShareError::Io)?;
                for entry in read_dir {
                    let entry = entry.map_err(RadShareError::Io)?;
                    let p = entry.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        let tag = store
                            .add_path(&p)
                            .temp_tag()
                            .await
                            .map_err(|e| RadShareError::Share(
                                radicle_artifact_share::Error::Serve(format!("add file: {e}")),
                            ))?;
                        let name = p
                            .strip_prefix(path)
                            .expect("path is under dir")
                            .to_string_lossy()
                            .into_owned();
                        entries.push((name, tag.hash()));
                    }
                }
            }
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));

            let collection = iroh_blobs::format::collection::Collection::from_iter(entries);
            let tag = collection
                .store(&store)
                .await
                .map_err(|e| RadShareError::Share(
                    radicle_artifact_share::Error::Serve(format!("store collection: {e}")),
                ))?;
            Ok::<_, RadShareError>(radicle_artifact_share::blake3_hash_to_cid(
                tag.hash(),
                radicle_artifact_share::ArtifactKind::Collection,
            ))
        })?;
        println!("{cid}");
    } else {
        let data = std::fs::read(path).map_err(RadShareError::Io)?;
        let hash = iroh_blobs::Hash::new(&data);
        let cid = radicle_artifact_share::blake3_hash_to_cid(
            hash,
            radicle_artifact_share::ArtifactKind::Blob,
        );
        println!("{cid}");
    }
    Ok(())
}

fn run_fetch(args: FetchArgs) -> Result<(), RadShareError> {
    let profile = Profile::load().map_err(RadShareError::Profile)?;
    let repo = open_repo(args.repository, &profile)?;
    let releases = Releases::open(&repo).map_err(|err| RadShareError::Releases {
        rid: repo.id,
        err,
    })?;

    let (oid, cid) = match (args.oid, args.cid) {
        (Some(oid), Some(cid)) => (oid, cid),
        (None, None) => pick_interactive(&releases)?,
        _ => {
            return Err(RadShareError::Usage(
                "provide both <oid> and <cid>, or neither for interactive mode".into(),
            ))
        }
    };

    // Find the release and artifact.
    let release_id = find_unique_by_oid(oid, &releases)?;
    let release = releases
        .get(&release_id)
        .map_err(|err| RadShareError::Find(FindError::Lookup { oid, err }))?
        .ok_or(RadShareError::Find(FindError::NoRelease(oid)))?;

    let artifact = release
        .artifact(&cid)
        .ok_or(RadShareError::ArtifactNotFound(cid))?;

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
    let output_path = args.output.unwrap_or_else(|| {
        let name = artifact.name();
        // Include CID in default filename to avoid collisions across artifacts.
        PathBuf::from(format!("{name}_{cid}"))
    });

    let preset = EndpointPreset::from_env().map_err(RadShareError::Share)?;

    match artifact_kind(&cid).map_err(RadShareError::Share)? {
        ArtifactKind::Blob => {
            let mut file = File::create(&output_path).map_err(RadShareError::Io)?;
            let fetchers = default_fetchers();
            download(&locations, &cid, &mut file, &fetchers, &preset)
                .map_err(RadShareError::Share)?;
        }
        ArtifactKind::Collection => {
            download_collection(&locations, &cid, &output_path, &preset)
                .map_err(RadShareError::Share)?;
        }
    }

    println!("{}", output_path.display());
    Ok(())
}

fn run_serve(args: ServeArgs) -> Result<(), RadShareError> {
    let profile = Profile::load().map_err(RadShareError::Profile)?;
    let repo = open_repo(args.repository, &profile)?;
    let mut releases = Releases::open(&repo).map_err(|err| RadShareError::Releases {
        rid: repo.id,
        err,
    })?;

    // Resolve the artifact, either from args or interactive picker.
    let cid = match args.cid {
        Some(cid) => cid,
        None => {
            let (_oid, cid) = pick_interactive(&releases)?;
            cid
        }
    };

    // Find the release containing this artifact.
    let (release_id, release) = releases
        .find_by_cid(&cid)
        .map_err(|e| RadShareError::Repository(e.to_string()))?
        .ok_or(RadShareError::ArtifactNotFound(cid))?;

    let artifact = release.artifact(&cid).expect("find_by_cid guarantees this");
    let kind = artifact_kind(&cid).map_err(RadShareError::Share)?;

    eprintln!("Artifact: {} (CID: {cid})", artifact.name());

    // Derive iroh secret key from radicle profile key.
    let iroh_sk = radicle_to_iroh_secret_key(&profile)?;
    let preset = EndpointPreset::from_env().map_err(RadShareError::Share)?;

    // Start the iroh-blobs server and add the content.
    let rt = tokio::runtime::Runtime::new().map_err(RadShareError::Io)?;
    rt.block_on(async {
        let server = Server::start(iroh_sk, preset)
            .await
            .map_err(RadShareError::Share)?;

        // Add content and verify CID matches.
        match kind {
            ArtifactKind::Blob => {
                add_blob(server.store(), &args.path, &cid)
                    .await
                    .map_err(RadShareError::Share)?;
            }
            ArtifactKind::Collection => {
                add_collection(server.store(), &args.path, &cid)
                    .await
                    .map_err(RadShareError::Share)?;
            }
        }

        // Build the iroh:// URL from our endpoint's public key.
        let endpoint_id = server.endpoint().id();
        let iroh_url = url::Url::parse(&format!("iroh://{endpoint_id}"))
            .map_err(|e| RadShareError::Usage(format!("failed to build iroh URL: {e}")))?;

        // Register the location in the release COB.
        let signer = profile.signer().map_err(RadShareError::Signer)?;
        let mut release_mut = releases
            .get_mut(&release_id)
            .map_err(|e| RadShareError::Repository(e.to_string()))?;
        release_mut
            .add_location(cid, iroh_url.clone(), &signer)
            .map_err(|e| RadShareError::Repository(e.to_string()))?;

        eprintln!("Serving at {iroh_url}");
        eprintln!("Press Ctrl+C to stop");

        tokio::signal::ctrl_c()
            .await
            .map_err(RadShareError::Io)?;

        eprintln!("\nShutting down...");
        server.shutdown().await.map_err(RadShareError::Share)?;

        Ok(())
    })
}

/// Convert artifact locations into fetch locations.
///
/// For `iroh://` URLs, derives the endpoint ID from the DID that
/// authored the location (same Ed25519 key).
/// For all other URLs, passes them through as-is.
fn artifact_locations(artifact: &Artifact) -> Result<Vec<Location<'_>>, RadShareError> {
    let mut locations = Vec::new();
    for (did, urls) in artifact.locations() {
        for url in urls {
            if url.scheme() == "iroh" {
                let pk_bytes: &[u8] = did.as_ref();
                let bytes: [u8; 32] = pk_bytes
                    .try_into()
                    .expect("ed25519 public key is 32 bytes");
                let pk = iroh::PublicKey::from_bytes(&bytes).map_err(|e| {
                    RadShareError::Usage(format!("invalid iroh endpoint ID from DID: {e}"))
                })?;
                locations.push(Location::Iroh(pk));
            } else {
                locations.push(Location::Url(url));
            }
        }
    }
    Ok(locations)
}

/// Derive an iroh secret key from the radicle profile's signing key.
///
/// Both use Ed25519, so the same 32-byte seed produces matching keys:
/// the iroh endpoint ID will equal the radicle DID's public key.
fn radicle_to_iroh_secret_key(profile: &Profile) -> Result<iroh::SecretKey, RadShareError> {
    let passphrase = radicle::profile::env::passphrase();
    let sk = profile
        .keystore
        .secret_key(passphrase)
        .map_err(|e| RadShareError::Usage(format!("failed to load signing key: {e}")))?
        .ok_or_else(|| RadShareError::Usage("signing key is encrypted; set RAD_PASSPHRASE".into()))?;
    let seed = sk.seed();
    let seed_bytes: &[u8; 32] = &*seed;
    Ok(iroh::SecretKey::from_bytes(seed_bytes))
}

fn open_repo(
    repository: Option<RepoId>,
    profile: &Profile,
) -> Result<Repository, RadShareError> {
    let repo_id = if let Some(repo_id) = repository {
        repo_id
    } else {
        let (_repo, repo_id) =
            radicle::rad::cwd().map_err(|e| RadShareError::Repository(e.to_string()))?;
        repo_id
    };
    profile
        .storage
        .repository(repo_id)
        .map_err(|e| RadShareError::Repository(e.to_string()))
}

fn find_unique_by_oid(
    oid: Oid,
    releases: &Releases<Repository>,
) -> Result<ReleaseId, RadShareError> {
    let mut iter = releases
        .find_by_oid(oid)
        .map_err(|err| RadShareError::Find(FindError::Lookup { oid, err }))?;
    let (id, _release) = iter
        .next()
        .ok_or(RadShareError::Find(FindError::NoRelease(oid)))?
        .map_err(|err| RadShareError::Find(FindError::Lookup { oid, err }))?;

    if iter.next().is_some() {
        return Err(RadShareError::Find(FindError::Ambiguous(oid)));
    }
    Ok(id)
}

/// Interactive mode: list releases, pick one, list its artifacts, pick one.
fn pick_interactive(
    releases: &Releases<Repository>,
) -> Result<(Oid, Cid), RadShareError> {
    let all: Vec<(ReleaseId, Release)> = releases
        .all()
        .map_err(|e| RadShareError::Repository(e.to_string()))?
        .filter_map(|res| res.ok())
        .map(|(id, release)| (ReleaseId::from(id), release))
        .collect();

    if all.is_empty() {
        return Err(RadShareError::Usage(
            "no releases found in this repository".into(),
        ));
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
        return Err(RadShareError::Usage(
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
        eprintln!(
            "  [{}] {} (CID: {}){}",
            i + 1,
            artifact.name(),
            cid,
            redacted
        );
    }

    let artifact_idx = prompt_choice("Select artifact", artifacts.len())?;
    let (cid, _) = artifacts[artifact_idx];

    Ok((*release.oid(), *cid))
}

/// Prompt user for a 1-indexed choice, return 0-indexed.
fn prompt_choice(label: &str, max: usize) -> Result<usize, RadShareError> {
    let stdin = io::stdin();
    loop {
        eprint!("{label} [1-{max}]: ");
        io::stderr().flush().map_err(RadShareError::Io)?;

        let mut line = String::new();
        stdin.lock().read_line(&mut line).map_err(RadShareError::Io)?;

        match line.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= max => return Ok(n - 1),
            _ => eprintln!("Invalid choice, try again."),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum RadShareError {
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

    #[error("artifact with CID {0} not found")]
    ArtifactNotFound(Cid),

    #[error("{0}")]
    Usage(String),

    #[error(transparent)]
    Share(radicle_artifact_share::Error),

    #[error("failed to get signer")]
    Signer(#[source] radicle::profile::SignerError),

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
