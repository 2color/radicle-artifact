//! A program to create and inspect artifact release COBs for a repository.
//!
//! Run `rad-artifact --help` to see how to use the program.

use std::{collections::BTreeSet, error::Error as _, io::IsTerminal, time::Duration};

use clap::Parser;

use radicle::{
    cob, crypto,
    crypto::signature::Signer,
    git::Oid,
    node::{
        device::Device,
        sync::{Announcer, AnnouncerConfig, ReplicationFactor},
        AliasStore, Handle, Node,
    },
    prelude::{Profile, ReadRepository, ReadStorage, RepoId, WriteRepository},
    profile,
    storage::git::Repository,
};
#[cfg(feature = "share")]
use radicle_artifact::share;
use radicle_artifact::*;

const TIMEOUT: Duration = Duration::from_millis(5000);

fn main() {
    if let Err(err) = fallible_main() {
        // Color "ERROR" red when stderr is a terminal and NO_COLOR is not set.
        let use_color = std::io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        if use_color {
            eprintln!("\x1b[1;31mERROR\x1b[0m: {err}");
        } else {
            eprintln!("ERROR: {err}");
        }
        let mut err = err.source();
        while let Some(underlying) = err {
            eprintln!("caused by: {underlying}");
            err = underlying.source();
        }
        std::process::exit(1);
    }
}

fn fallible_main() -> Result<(), RadArtifactError> {
    let args = Args::parse();
    run(args)
}

/// Create, update, and query Radicle artifact release COBs.
///
/// An artifact release COB records content-addressed artifacts associated
/// with a Git commit or annotated tag, along with discovery locations where
/// each artifact can be retrieved.
///
/// Output is human-readable in a terminal, JSON when piped.
/// Use --json or --pretty to override.
#[derive(Parser)]
#[clap(version)]
struct Args {
    /// Use this repository. Default is the current working directory.
    #[clap(short, long)]
    repository: Option<RepoId>,

    /// Do not sync with the network after modifications.
    ///
    /// Note that if `--no-sync` was used, you can use `rad sync -a` to announce
    /// at a later point.
    #[clap(long)]
    no_sync: bool,

    /// Disable all interactive prompts.
    ///
    /// Commands that would normally prompt (e.g. `fetch` without arguments)
    /// will error instead. Useful for scripts and CI.
    #[clap(long)]
    no_input: bool,

    #[clap(subcommand)]
    command: command::Command,
}

impl Args {
    fn repository(&self, profile: &Profile) -> Result<Repository, error::Repository> {
        let repo_id = if let Some(repo_id) = self.repository {
            repo_id
        } else {
            let (_repo, repo_id) = radicle::rad::cwd().map_err(error::Repository::Cwd)?;
            repo_id
        };
        profile
            .storage
            .repository(repo_id)
            .map_err(|err| error::Repository::Open { rid: repo_id, err })
    }
}

fn load_profile() -> Result<Profile, error::Profile> {
    Profile::load().map_err(error::Profile)
}

fn open_releases(repo: &Repository) -> Result<Releases<'_, Repository>, error::Releases> {
    Releases::open(repo).map_err(|err| error::Releases { rid: repo.id, err })
}

fn announce(profile: &Profile, repo_id: RepoId) -> Result<(), error::Announce> {
    let mut node = Node::new(profile.home.socket());

    // Check seed sync status for the local node's namespace, matching the
    // behavior of the deprecated `seeds()` method which passed `[self.nid()]`.
    let local_id = *profile.id();
    let (synced, unsynced) = node
        .seeds_for(repo_id, [local_id])
        .map_err(error::Announce::Seeds)?
        .iter()
        .fold(
            (BTreeSet::new(), BTreeSet::new()),
            |(mut synced, mut unsynced), seed| {
                if seed.is_synced() {
                    synced.insert(seed.nid);
                } else {
                    unsynced.insert(seed.nid);
                }
                (synced, unsynced)
            },
        );

    let announcer = Announcer::new(AnnouncerConfig::public(
        *profile.id(),
        ReplicationFactor::MustReach(1),
        BTreeSet::new(),
        synced,
        unsynced,
    ))
    .map_err(error::Announce::Announcer)?;

    // Announce refs for the local node's namespace only.
    node.announce(repo_id, [local_id], TIMEOUT, announcer, |_, _| ())
        .map_err(error::Announce::Announcement)?;

    Ok(())
}

fn run(args: Args) -> Result<(), RadArtifactError> {
    use command::*;

    // The Cid subcommand doesn't need a profile or repo.
    #[cfg(feature = "share")]
    if let Command::ComputeCid(cmd) = args.command {
        return run_cid(cmd);
    }

    let profile = load_profile()?;
    let repo = args.repository(&profile)?;
    let mut releases = open_releases(&repo)?;
    match args.command {
        #[cfg(feature = "share")]
        Command::ComputeCid(_) => unreachable!(), // handled above
        Command::Add(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            add_artifact(cmd, &mut releases, &repo, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Location(loc) => {
            let signer = profile.signer().map_err(error::Signer)?;
            match loc.command {
                LocationCommand::Add(cmd) => {
                    location_add(cmd, &mut releases, &repo, &signer)?;
                }
                LocationCommand::Remove(cmd) => {
                    location_remove(cmd, &mut releases, &repo, &signer)?;
                }
            }
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Attest(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            attest_artifact(cmd, &mut releases, &repo, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Redact(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            redact_artifact(cmd, &mut releases, &repo, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Show(cmd) => show_release(cmd, &releases, &repo, &profile)?,
        Command::List(cmd) => list_releases(cmd, &releases, &repo, &profile)?,
        #[cfg(feature = "share")]
        Command::Fetch(cmd) => run_fetch(cmd, args.no_input, &profile, &releases, &repo)?,
        #[cfg(feature = "share")]
        Command::Serve(cmd) => run_serve(cmd, args.no_input, &profile, &mut releases, &repo)?,
    }

    Ok(())
}

fn add_artifact<G>(
    command::Add { commit, cid, name }: command::Add,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    signer: &Device<G>,
) -> Result<(), error::Add>
where
    G: Signer<crypto::Signature>,
{
    let oid = resolve_commit(&commit, repo)?;
    let mut release = releases
        .find_or_create_by_oid(oid, signer)
        .map_err(|err| error::Add::FindOrCreate { oid, err })?;
    let id = *release.id();
    release
        .add_artifact(cid, name.clone(), signer)
        .map_err(|err| error::Add::Store { id, err })?;
    eprintln!("Added artifact '{name}' to release {}", &oid.to_string()[..7]);
    if std::io::stderr().is_terminal() {
        eprintln!("Hint: use `rad-artifact location add {commit} --cid {cid} <url>` to add a download location");
    }
    println!("{cid}");
    Ok(())
}

fn location_add<G>(
    command::LocationAdd { commit, cid, url }: command::LocationAdd,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    signer: &Device<G>,
) -> Result<(), error::Locate>
where
    G: Signer<crypto::Signature>,
{
    let oid = resolve_commit(&commit, repo)?;
    let id = find_unique_by_oid(oid, releases)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Locate::Store { id, err })?;
    release
        .add_location(cid, url.clone(), signer)
        .map_err(|err| error::Locate::Store { id, err })?;
    eprintln!("Added location {url} for artifact {cid}");
    if std::io::stderr().is_terminal() {
        eprintln!("Hint: use `rad-artifact show --pretty {commit}` to verify the release");
    }
    Ok(())
}

fn attest_artifact<G>(
    command::Attest { commit, cid }: command::Attest,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    signer: &Device<G>,
) -> Result<(), error::Attest>
where
    G: Signer<crypto::Signature>,
{
    let oid = resolve_commit(&commit, repo)?;
    let id = find_unique_by_oid(oid, releases)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Attest::Store { id, err })?;
    release
        .attest(cid, signer)
        .map_err(|err| error::Attest::Store { id, err })?;
    eprintln!("Attested artifact {cid}");
    Ok(())
}

fn redact_artifact<G>(
    command::Redact { commit, cid, reason }: command::Redact,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    signer: &Device<G>,
) -> Result<(), error::Redact>
where
    G: Signer<crypto::Signature>,
{
    let oid = resolve_commit(&commit, repo)?;
    let id = find_unique_by_oid(oid, releases)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Redact::Store { id, err })?;
    release
        .redact(cid, reason, signer)
        .map_err(|err| error::Redact::Redact { id, err })?;
    eprintln!("redacted {cid}");
    Ok(())
}

fn location_remove<G>(
    command::LocationRemove { commit, cid, url }: command::LocationRemove,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    signer: &Device<G>,
) -> Result<(), error::RemoveLocation>
where
    G: Signer<crypto::Signature>,
{
    let oid = resolve_commit(&commit, repo)?;
    let id = find_unique_by_oid(oid, releases)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::RemoveLocation::Store { id, err })?;
    release
        .remove_location(cid, url.clone(), signer)
        .map_err(|err| error::RemoveLocation::Store { id, err })?;
    eprintln!("Removed location {url} for artifact {cid}");
    Ok(())
}

/// Decide whether to use pretty output: --pretty wins, --json wins, otherwise
/// auto-detect based on whether stdout is a TTY.
fn use_pretty(pretty: bool, json: bool) -> bool {
    if json {
        return false;
    }
    if pretty {
        return true;
    }
    std::io::stdout().is_terminal()
}

fn show_release(
    command::Show {
        pretty,
        json,
        redacted,
        commit,
    }: command::Show,
    releases: &Releases<Repository>,
    repo: &Repository,
    aliases: &impl AliasStore,
) -> Result<(), error::Show> {
    let oid = resolve_commit(&commit, repo)?;
    let delegates = if redacted {
        None
    } else {
        let ds: BTreeSet<_> = repo
            .delegates()
            .map_err(error::Show::Delegates)?
            .into_iter()
            .collect();
        Some(ds)
    };
    let id = find_unique_by_oid(oid, releases)?;
    let release = releases
        .get(&id)
        .map_err(|err| error::Find::Lookup { oid, err })?
        .ok_or(error::Find::NoRelease(oid))?;
    let title = display::CommitTitle::title(repo, release.oid());
    let show =
        radicle_artifact::display::Release::new(id, &release, aliases, delegates.as_ref(), title);
    if use_pretty(pretty, json) {
        println!("{}", show.pretty());
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&show).map_err(error::Show::Json)?
        );
    }
    Ok(())
}

fn list_releases(
    command::List {
        pretty,
        json,
        verbose,
        delegates_only,
        redacted,
        empty,
    }: command::List,
    releases: &Releases<Repository>,
    repo: &Repository,
    aliases: &impl AliasStore,
) -> Result<(), error::List> {
    // Fetch delegates when needed for --delegates-only or redaction filtering.
    let delegates = if delegates_only || !redacted {
        let ds: BTreeSet<_> = repo
            .delegates()
            .map_err(error::List::Delegates)?
            .into_iter()
            .collect();
        Some(ds)
    } else {
        None
    };
    let iter = releases
        .all()
        .map_err(error::List::All)?
        .filter_map(|res| match res {
            Ok((id, release)) => Some((ReleaseId::from(id), release)),
            Err(err) => {
                if verbose {
                    eprintln!("Failed to retrieve release: {err}");
                }
                None
            }
        })
        .filter({
            let delegates = delegates.clone();
            move |(_id, release)| {
                if delegates_only {
                    delegates
                        .as_ref()
                        .map_or(true, |ds| ds.contains(release.author()))
                } else {
                    true
                }
            }
        });
    // Pass delegates for redaction filtering only when --redacted is not set.
    let redaction_filter = if redacted { None } else { delegates.as_ref() };
    let releases = display::Releases::new(iter, aliases, redaction_filter, empty, repo);
    if use_pretty(pretty, json) {
        println!("{}", releases.pretty());
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&releases).map_err(error::List::Json)?
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Share commands (behind "share" feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "share")]
fn run_cid(args: command::ComputeCid) -> Result<(), RadArtifactError> {
    let path = &args.path;
    if path.is_dir() {
        let cid = share::compute_content_id(path).map_err(error::Share::Io)?;
        println!("{cid}");
    } else {
        let data = std::fs::read(path).map_err(error::Share::Io)?;
        let hash = iroh_blobs::Hash::new(&data);
        let cid = share::blake3_hash_to_cid(hash, share::ArtifactKind::Blob);
        println!("{cid}");
    }
    Ok(())
}

#[cfg(feature = "share")]
fn run_fetch(
    args: command::Fetch,
    no_input: bool,
    _profile: &Profile,
    releases: &Releases<Repository>,
    repo: &Repository,
) -> Result<(), RadArtifactError> {
    // clap's `requires` ensures both or neither are provided.
    let (oid, cid) = match (args.commit, args.cid) {
        (Some(commit), Some(cid)) => {
            let oid = resolve_commit(&commit, repo)?;
            (oid, cid)
        }
        (None, None) => pick_interactive(no_input, releases, repo)?,
        _ => unreachable!("clap enforces both-or-neither"),
    };

    let release_id = find_unique_by_oid(oid, releases)?;
    let release = releases
        .get(&release_id)
        .map_err(|err| error::Find::Lookup { oid, err })?
        .ok_or(error::Find::NoRelease(oid))?;

    let artifact = release
        .artifact(&cid)
        .ok_or(error::Share::ArtifactNotFound(cid))?;

    if artifact.is_redacted() {
        eprintln!("WARNING: this artifact has been redacted");
        for (did, reason) in artifact.redactions() {
            eprintln!("  {did}: {reason}");
        }
    }

    eprintln!("Artifact: {} (CID: {cid})", artifact.name());

    let locations = if let Some(ref url) = args.url {
        vec![share::Location::Url(url)]
    } else {
        artifact_locations(artifact)?
    };
    eprintln!(
        "Trying {} location{}...",
        locations.len(),
        if locations.len() == 1 { "" } else { "s" }
    );

    let output_path = args.output.unwrap_or_else(|| {
        let name = artifact.name();
        std::path::PathBuf::from(format!("{}_{cid}", name.replace(' ', "_")))
    });

    let preset = share::EndpointPreset::from_env().map_err(error::Share::Share)?;
    let kind = share::artifact_kind(&cid).map_err(error::Share::Share)?;

    match kind {
        share::ArtifactKind::Blob => {
            let fetchers = share::default_fetchers();
            share::download(&locations, &cid, &output_path, &fetchers, &preset)
                .map_err(error::Share::Share)?;
        }
        share::ArtifactKind::Collection => {
            share::download_collection(&locations, &cid, &output_path, &preset)
                .map_err(error::Share::Share)?;
        }
    }

    eprintln!("Saved to {}", output_path.display());
    Ok(())
}

#[cfg(feature = "share")]
fn run_serve(
    args: command::Serve,
    no_input: bool,
    profile: &Profile,
    releases: &mut Releases<Repository>,
    repo: &Repository,
) -> Result<(), RadArtifactError> {
    let cid = match args.cid {
        Some(cid) => cid,
        None => {
            let (_oid, cid) = pick_interactive(no_input, releases, repo)?;
            cid
        }
    };

    let (release_id, release) = releases
        .find_by_cid(&cid)
        .map_err(|e| error::Share::Usage(e.to_string()))?
        .ok_or(error::Share::ArtifactNotFound(cid))?;

    let artifact = release.artifact(&cid).expect("find_by_cid guarantees this");
    let kind = share::artifact_kind(&cid).map_err(error::Share::Share)?;

    eprintln!("Artifact: {} (CID: {cid})", artifact.name());

    let passphrase = radicle::profile::env::passphrase();
    let iroh_sk = share::radicle_secret_to_iroh(&profile.keystore, passphrase)
        .map_err(error::Share::Share)?;
    let preset = share::EndpointPreset::from_env().map_err(error::Share::Share)?;

    let rt = tokio::runtime::Runtime::new().map_err(error::Share::Io)?;
    rt.block_on(async {
        let server = share::Server::start(iroh_sk, preset)
            .await
            .map_err(error::Share::Share)?;

        match kind {
            share::ArtifactKind::Blob => {
                share::add_blob(server.store(), &args.path, &cid)
                    .await
                    .map_err(error::Share::Share)?;
            }
            share::ArtifactKind::Collection => {
                share::add_collection(server.store(), &args.path, &cid)
                    .await
                    .map_err(error::Share::Share)?;
            }
        }

        let endpoint_id = server.endpoint().id();
        let iroh_url = url::Url::parse(&format!("iroh://{endpoint_id}"))
            .map_err(|e| error::Share::Usage(format!("failed to build iroh URL: {e}")))?;

        let signer = profile.signer().map_err(error::Signer)?;
        let mut release_mut = releases
            .get_mut(&release_id)
            .map_err(|e| error::Share::Usage(e.to_string()))?;
        release_mut
            .add_location(cid, iroh_url.clone(), &signer)
            .map_err(|e| error::Share::Usage(e.to_string()))?;

        eprintln!("Serving at {iroh_url}");
        eprintln!("Press Ctrl+C to stop");

        tokio::signal::ctrl_c().await.map_err(error::Share::Io)?;

        eprintln!("\nShutting down...");
        server.shutdown().await.map_err(error::Share::Share)?;

        Ok::<_, RadArtifactError>(())
    })
}

/// Convert artifact locations into fetch locations.
///
/// For `iroh://` URLs, derives the endpoint ID from the DID that
/// authored the location (same Ed25519 key).
#[cfg(feature = "share")]
fn artifact_locations(artifact: &Artifact) -> Result<Vec<share::Location<'_>>, RadArtifactError> {
    let mut locations = Vec::new();
    for (did, urls) in artifact.locations() {
        for url in urls {
            if url.scheme() == "iroh" {
                let pk = share::did_to_iroh_public_key(did).map_err(error::Share::Share)?;
                locations.push(share::Location::Iroh(pk));
            } else {
                locations.push(share::Location::Url(url));
            }
        }
    }
    Ok(locations)
}

/// Interactive mode: list releases, pick one, list its artifacts, pick one.
///
/// Requires stdin to be a TTY. Errors if `no_input` is set or stdin is not
/// interactive, so scripts don't hang waiting for input.
#[cfg(feature = "share")]
fn pick_interactive(
    no_input: bool,
    releases: &Releases<Repository>,
    repo: &Repository,
) -> Result<(Oid, radicle_artifact::Cid), RadArtifactError> {
    if no_input || !std::io::stdin().is_terminal() {
        return Err(error::Share::Usage(
            "interactive mode requires a terminal; pass <commit> and --cid arguments, or remove --no-input".into(),
        )
        .into());
    }
    let all: Vec<(ReleaseId, Release)> = releases
        .all()
        .map_err(|e| error::Share::Usage(e.to_string()))?
        .filter_map(|res| res.ok())
        .map(|(id, release)| (ReleaseId::from(id), release))
        .collect();

    if all.is_empty() {
        return Err(error::Share::Usage("no releases found in this repository".into()).into());
    }

    eprintln!("Releases:");
    for (i, (_, release)) in all.iter().enumerate() {
        let oid = release.oid();
        let short = &oid.to_string()[..7];
        let title = display::CommitTitle::title(repo, oid).unwrap_or_default();
        let artifact_count = release.artifacts().len();
        eprintln!(
            "  [{}] {} {} ({} artifact{})",
            i + 1,
            short,
            title,
            artifact_count,
            if artifact_count == 1 { "" } else { "s" }
        );
    }

    let release_idx = prompt_choice("Select release", all.len())?;
    let (_, release) = &all[release_idx];

    let artifacts: Vec<(&radicle_artifact::Cid, &Artifact)> = release.artifacts().iter().collect();
    if artifacts.is_empty() {
        return Err(error::Share::Usage("selected release has no artifacts".into()).into());
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
#[cfg(feature = "share")]
fn prompt_choice(label: &str, max: usize) -> Result<usize, RadArtifactError> {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    loop {
        eprint!("{label} [1-{max}]: ");
        std::io::stderr().flush().map_err(error::Share::Io)?;

        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .map_err(error::Share::Io)?;

        match line.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= max => return Ok(n - 1),
            _ => eprintln!("Invalid choice, try again."),
        }
    }
}

/// Resolve a commit reference (full OID, short OID, or tag name) to an [`Oid`].
fn resolve_commit(commit: &str, repo: &Repository) -> Result<Oid, error::Resolve> {
    let object = repo
        .raw()
        .revparse_single(commit)
        .map_err(|err| error::Resolve {
            commit: commit.to_owned(),
            err,
        })?;
    Ok(object.id().into())
}

/// Find the unique release for a given OID. Errors if none or more than one exist.
fn find_unique_by_oid(oid: Oid, releases: &Releases<Repository>) -> Result<ReleaseId, error::Find> {
    let mut iter = releases
        .find_by_oid(oid)
        .map_err(|err| error::Find::Lookup { oid, err })?;
    let (id, _release) = iter
        .next()
        .ok_or(error::Find::NoRelease(oid))?
        .map_err(|err| error::Find::Lookup { oid, err })?;

    // Check for ambiguity — multiple releases for the same OID.
    if iter.next().is_some() {
        return Err(error::Find::Ambiguous(oid));
    }

    Ok(id)
}

#[derive(Debug, thiserror::Error)]
enum RadArtifactError {
    #[error(transparent)]
    Profile(#[from] error::Profile),
    #[error(transparent)]
    Signer(#[from] error::Signer),
    #[error(transparent)]
    Repository(#[from] error::Repository),
    #[error(transparent)]
    Releases(#[from] error::Releases),
    #[error(transparent)]
    Announce(#[from] error::Announce),
    #[error(transparent)]
    Show(#[from] error::Show),
    #[error(transparent)]
    List(#[from] error::List),
    #[error(transparent)]
    Add(#[from] error::Add),
    #[error(transparent)]
    Locate(#[from] error::Locate),
    #[error(transparent)]
    RemoveLocation(#[from] error::RemoveLocation),
    #[error(transparent)]
    Attest(#[from] error::Attest),
    #[error(transparent)]
    Redact(#[from] error::Redact),
    #[error(transparent)]
    Find(#[from] error::Find),
    #[error(transparent)]
    Resolve(#[from] error::Resolve),
    #[cfg(feature = "share")]
    #[error(transparent)]
    Share(#[from] error::Share),
}

mod command {
    use clap::Parser;
    use url::Url;

    use radicle_artifact::Cid;

    #[derive(Parser)]
    pub enum Command {
        Add(Add),
        /// Manage discovery locations for artifacts.
        Location(Location),
        Attest(Attest),
        Redact(Redact),
        Show(Show),
        List(List),
        /// Compute the BLAKE3 CID of a file or directory
        #[cfg(feature = "share")]
        #[clap(name = "cid")]
        ComputeCid(ComputeCid),
        /// Fetch an artifact from a release COB
        #[cfg(feature = "share")]
        Fetch(Fetch),
        /// Serve an artifact via iroh-blobs using your radicle identity
        #[cfg(feature = "share")]
        Serve(Serve),
    }

    /// Manage discovery locations for artifacts.
    ///
    /// Locations announce where an artifact can be retrieved from.
    #[derive(Parser)]
    pub struct Location {
        #[clap(subcommand)]
        pub command: LocationCommand,
    }

    #[derive(Parser)]
    pub enum LocationCommand {
        Add(LocationAdd),
        Remove(LocationRemove),
    }

    /// Compute the BLAKE3 CID of a file or directory.
    ///
    /// For a single file, outputs a CID with the raw codec (0x55).
    /// For a directory, outputs a CID with the blake3-hashseq codec (0x80).
    #[cfg(feature = "share")]
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Compute CID of a file:
    $ rad-artifact cid ./my-binary

  Compute CID of a directory:
    $ rad-artifact cid ./dist/")]
    pub struct ComputeCid {
        /// Path to file or directory.
        pub path: std::path::PathBuf,
    }

    /// Fetch an artifact from a Radicle release COB.
    ///
    /// With positional arguments, fetches a specific artifact directly.
    /// Without arguments, interactively lists releases and artifacts to pick from.
    #[cfg(feature = "share")]
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact fetch

  Fetch a specific artifact:
    $ rad-artifact fetch abc1234 --cid baf...abc

  Fetch to a custom path:
    $ rad-artifact fetch abc1234 --cid baf...abc -o ./downloads/my-binary

  Fetch from a specific URL:
    $ rad-artifact fetch abc1234 --cid baf...abc --url https://example.com/my-binary")]
    pub struct Fetch {
        /// Git commit, tag, or abbreviated OID. Required with --cid.
        #[clap(requires = "cid")]
        pub commit: Option<String>,
        /// Content identifier of the artifact to fetch. Required with <COMMIT>.
        #[clap(long, requires = "commit")]
        pub cid: Option<radicle_artifact::Cid>,
        /// Output file path. Defaults to the artifact name in the current directory.
        #[clap(short, long)]
        pub output: Option<std::path::PathBuf>,
        /// Fetch from this URL directly, skipping registered locations.
        #[clap(long)]
        pub url: Option<url::Url>,
    }

    /// Serve an artifact via iroh-blobs using your radicle identity.
    ///
    /// Verifies the file/directory matches the artifact CID, registers an
    /// `iroh://<endpoint_id>` location in the release COB, and serves the
    /// content until interrupted.
    #[cfg(feature = "share")]
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Serve with interactive artifact picker:
    $ rad-artifact serve ./my-binary

  Serve a specific artifact:
    $ rad-artifact serve ./my-binary --cid baf...abc")]
    pub struct Serve {
        /// Path to file or directory to serve.
        pub path: std::path::PathBuf,
        /// Artifact CID. If omitted, launches interactive picker.
        #[clap(long)]
        pub cid: Option<radicle_artifact::Cid>,
    }

    /// Add an artifact to a release, creating it if needed.
    ///
    /// The artifact is identified by its content identifier (CID).
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Compute the CID and add a release artifact:
    $ rad-artifact cid ./my-binary
    $ rad-artifact add abc1234 --cid baf...abc -n \"my-binary v1.0\"

  Add an artifact for another commit:
    $ rad-artifact add def5678 --cid baf...abc --name \"my-binary v1.0\"")]
    pub struct Add {
        /// Git commit, tag, or abbreviated OID of the release.
        pub commit: String,
        /// Content identifier for the artifact.
        #[clap(long)]
        pub cid: Cid,
        /// Human-readable description of the artifact.
        #[clap(short, long)]
        pub name: String,
    }

    /// Add a discovery location for an artifact.
    ///
    /// Announces where an artifact can be retrieved from.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Register an HTTPS download location:
    $ rad-artifact location add abc1234 --cid baf...abc https://example.com/my-binary

  Register an iroh-blobs endpoint:
    $ rad-artifact location add abc1234 --cid baf...abc iroh://<endpoint-id>")]
    pub struct LocationAdd {
        /// Git commit, tag, or abbreviated OID of the release.
        pub commit: String,
        /// Content identifier for the artifact.
        #[clap(long)]
        pub cid: Cid,
        /// URL where the artifact can be retrieved.
        pub url: Url,
    }

    /// Attest that this node has independently verified an artifact.
    ///
    /// Records that the signing node built from the same commit and
    /// obtained the same CID. Idempotent — attesting twice is a no-op.
    #[derive(Parser)]
    pub struct Attest {
        /// Git commit, tag, or abbreviated OID of the release.
        pub commit: String,
        /// Content identifier for the artifact to attest.
        #[clap(long)]
        pub cid: Cid,
    }

    /// Redact an artifact, indicating it should not be used.
    ///
    /// Records that the signing node believes this artifact is compromised
    /// or should be withdrawn. The reason is a free-form string (max 2048
    /// bytes). The act of redaction is permanent; the reason text can be
    /// amended by redacting again. A redaction supersedes any prior
    /// attestation from the same DID.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Redact a compromised artifact:
    $ rad-artifact redact abc1234 --cid baf...abc -m \"build compromised, see advisory\"")]
    pub struct Redact {
        /// Git commit, tag, or abbreviated OID of the release.
        pub commit: String,
        /// Content identifier for the artifact to redact.
        #[clap(long)]
        pub cid: Cid,
        /// Reason for the redaction.
        #[clap(short = 'm', long = "reason")]
        pub reason: String,
    }

    /// Remove a discovery location for an artifact.
    ///
    /// Retracts a previously announced location.
    #[derive(Parser)]
    pub struct LocationRemove {
        /// Git commit, tag, or abbreviated OID of the release.
        pub commit: String,
        /// Content identifier for the artifact.
        #[clap(long)]
        pub cid: Cid,
        /// URL to remove.
        pub url: Url,
    }

    /// Show the release COB for a Git commit or annotated tag.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Show a release as JSON (default):
    $ rad-artifact show abc1234

  Show a release in human-readable format:
    $ rad-artifact show --pretty abc1234

  Include redacted artifacts:
    $ rad-artifact show --pretty --redacted abc1234")]
    pub struct Show {
        /// Format output in a human-readable way.
        ///
        /// This is the default when stdout is a terminal.
        #[clap(long)]
        pub pretty: bool,
        /// Force JSON output.
        ///
        /// This is the default when stdout is not a terminal (e.g. piped).
        #[clap(long, conflicts_with = "pretty")]
        pub json: bool,
        /// Also show artifacts that have been redacted by a trusted party.
        #[clap(long)]
        pub redacted: bool,
        /// Git commit, tag, or abbreviated OID of the release.
        pub commit: String,
    }

    /// List all release COBs for a repository.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  List all releases as JSON:
    $ rad-artifact list

  Human-readable listing of delegate releases:
    $ rad-artifact list --pretty --delegates-only

  Include empty and redacted releases:
    $ rad-artifact list --pretty --empty --redacted")]
    pub struct List {
        /// Format output in a human-readable way.
        ///
        /// This is the default when stdout is a terminal.
        #[clap(long)]
        pub pretty: bool,
        /// Force JSON output.
        ///
        /// This is the default when stdout is not a terminal (e.g. piped).
        #[clap(long, conflicts_with = "pretty")]
        pub json: bool,
        /// Output all information, including intermediate errors.
        #[clap(long, short)]
        pub verbose: bool,
        /// Only show releases created by delegates of the repository.
        #[clap(long)]
        pub delegates_only: bool,
        /// Also show artifacts that have been redacted by a trusted party.
        #[clap(long)]
        pub redacted: bool,
        /// Also show releases that have no artifacts.
        #[clap(long)]
        pub empty: bool,
    }
}

mod error {
    use radicle::{
        node::{self, sync::AnnouncerError},
        rad::CwdError,
        storage::RepositoryError,
    };
    use thiserror::Error;

    use super::*;

    #[derive(Debug, Error)]
    pub enum Show {
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("failed to get repository delegates")]
        Delegates(#[source] RepositoryError),
        #[error("failed to show release, could not serialize to JSON")]
        Json(#[source] serde_json::Error),
    }

    #[derive(Debug, Error)]
    pub enum List {
        #[error("failed to list releases")]
        All(#[source] cob::store::Error),
        #[error("failed to get repository delegates")]
        Delegates(#[source] RepositoryError),
        #[error("failed to list releases, could not serialize to JSON")]
        Json(#[source] serde_json::Error),
    }

    #[derive(Debug, Error)]
    pub enum Add {
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error("failed to find or create release for commit {oid}")]
        FindOrCreate {
            oid: Oid,
            #[source]
            err: radicle_artifact::FindOrCreateError,
        },
        #[error("failed to add artifact to release {id}")]
        Store {
            id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
    }

    #[derive(Debug, Error)]
    pub enum Locate {
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("failed to add location to release {id}")]
        Store {
            id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
    }

    #[derive(Debug, Error)]
    pub enum Attest {
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("failed to attest artifact in release {id}")]
        Store {
            id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
    }

    #[derive(Debug, Error)]
    pub enum Redact {
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("failed to redact artifact in release {id}")]
        Redact {
            id: ReleaseId,
            #[source]
            err: radicle_artifact::error::Redact,
        },
        #[error("failed to redact artifact in release {id}")]
        Store {
            id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
    }

    #[derive(Debug, Error)]
    pub enum RemoveLocation {
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("failed to remove location from release {id}")]
        Store {
            id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
    }

    #[derive(Debug, Error)]
    pub enum Find {
        #[error("no release was found for the commit {0}")]
        NoRelease(Oid),
        #[error("multiple releases found for the commit {0}, use a release ID to disambiguate")]
        Ambiguous(Oid),
        #[error("failed to find a release for the commit {oid}")]
        Lookup {
            oid: Oid,
            #[source]
            err: cob::store::Error,
        },
    }

    #[derive(Debug, Error)]
    #[error("could not resolve '{commit}' to a git object")]
    pub struct Resolve {
        pub commit: String,
        #[source]
        pub err: radicle::git::raw::Error,
    }

    #[derive(Debug, Error)]
    pub enum Repository {
        #[error("failed to find Radicle repository for current working directory")]
        Cwd(#[source] CwdError),
        #[error("failed to open Radicle repository {rid}")]
        Open {
            rid: RepoId,
            #[source]
            err: RepositoryError,
        },
    }

    #[derive(Debug, Error)]
    #[error("failed to load Radicle profile")]
    pub struct Profile(#[source] pub profile::Error);

    #[derive(Debug, Error)]
    #[error("failed to get the signing key of the Radicle profile")]
    pub struct Signer(#[source] pub profile::SignerError);

    #[derive(Debug, Error)]
    pub enum Announce {
        #[error("failed to get seeds for announcing changes")]
        Seeds(#[source] node::Error),
        #[error("failed to announce changes")]
        Announcer(AnnouncerError),
        #[error("failed to announce changes")]
        Announcement(#[source] node::Error),
    }

    #[derive(Debug, Error)]
    #[error("failed to open release store for {rid}")]
    pub struct Releases {
        pub rid: RepoId,
        #[source]
        pub err: RepositoryError,
    }

    #[cfg(feature = "share")]
    #[derive(Debug, Error)]
    pub enum Share {
        #[error("{0}")]
        Usage(String),
        #[error("artifact with CID {0} not found")]
        ArtifactNotFound(radicle_artifact::Cid),
        #[error(transparent)]
        Share(radicle_artifact::share::Error),
        #[error("I/O error")]
        Io(#[source] std::io::Error),
    }
}
