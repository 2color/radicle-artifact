//! A cli to create and inspect artifact release COBs for a repository.
//!
//! Run `rad-artifact --help` to see how to use the cli.

use std::{collections::BTreeSet, error::Error as _, io::IsTerminal, time::Duration};

use clap::Parser;

use radicle::{
    cob::{self, store::access::WriteAs},
    git::Oid,
    identity::Did,
    node::{
        AliasStore, Handle, Node, sync::{Announcer, AnnouncerConfig, ReplicationFactor}
    },
    prelude::{Profile, ReadRepository, ReadStorage, RepoId, WriteRepository},
    profile,
    storage::git::Repository,
};
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

fn open_releases<Access: cob::store::access::Access>(repo: &Repository, access: Access) -> Result<Releases<'_, Repository, Access>, error::Releases> {
    Releases::open(repo, access).map_err(|err| error::Releases { rid: repo.id, err })
}

fn repo_delegates(repo: &Repository) -> Result<BTreeSet<Did>, error::Delegates> {
    Ok(repo
        .delegates()
        .map_err(error::Delegates)?
        .into_iter()
        .collect())
}

fn announce(profile: &Profile, repo_id: RepoId) -> Result<(), error::Announce> {
    let mut node = Node::new(profile.home.socket_from_env());

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
    if let Command::ComputeCid(cmd) = args.command {
        return run_cid(cmd);
    }

    let profile = load_profile()?;
    let repo = args.repository(&profile)?;
    let signer = profile.signer().map_err(error::Signer)?;
    let mut releases = open_releases(&repo, WriteAs::new(&signer))?;
    match args.command {
        Command::ComputeCid(_) => unreachable!(), // handled above
        Command::Add(cmd) => {
            add_artifact(cmd, args.no_input, &mut releases, &repo)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Location(loc) => {
            match loc.command {
                LocationCommand::Add(cmd) => {
                    location_add(cmd, &mut releases, &repo)?;
                }
                LocationCommand::Remove(cmd) => {
                    location_remove(cmd, &mut releases, &repo)?;
                }
            }
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Attest(cmd) => {
            attest_artifact(cmd, args.no_input, &mut releases, &repo)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Redact(cmd) => {
            redact_artifact(cmd, args.no_input, &mut releases, &repo)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Show(cmd) => {
            let delegates = repo_delegates(&repo)?;
            show_release(cmd, &releases, &repo, &delegates, &profile)?;
        }
        Command::List(cmd) => list_releases(cmd, &releases, &repo, &profile)?,
        Command::Fetch(cmd) => run_fetch(cmd, args.no_input, &profile, &releases, &repo)?,
        Command::Serve(cmd) => run_serve(cmd, args.no_input, &profile, &mut releases)?,
    }

    Ok(())
}

fn add_artifact<Signer>(
    command::Add {
        path,
        cid,
        commit,
        name,
    }: command::Add,
    no_input: bool,
    releases: &mut Releases<Repository, WriteAs<Signer>>,
    repo: &Repository,
) -> Result<(), error::Add>
where
    Signer: radicle::crypto::signature::Signer<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Signer<radicle::crypto::ssh::ExtendedSignature>,
    Signer: radicle::crypto::signature::Verifier<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Keypair<VerifyingKey = radicle::crypto::PublicKey>,
{
    // Clap's ArgGroup enforces exactly one of `path` / `cid`, so exactly one
    // branch is taken here.
    let cid = match (path.as_deref(), cid) {
        (Some(path), None) => compute_cid_from_path(path)?,
        (None, Some(cid)) => cid,
        // ArgGroup guarantees this is unreachable, but we surface a clear
        // usage error rather than panic if clap ever changes its mind.
        (Some(_), Some(_)) => {
            return Err(error::Add::Usage(
                "cannot pass --cid together with a path; the CID is computed from the contents"
                    .into(),
            ));
        }
        (None, None) => {
            return Err(error::Add::Usage(
                "missing artifact source; pass a <PATH> or --cid <CID>".into(),
            ));
        }
    };

    // Resolve commit/tag: use --commit if given, otherwise prompt.
    let oid = match commit.as_deref() {
        Some(rev) => resolve_commit(rev, repo)?,
        None => prompt::pick_commit_or_tag(no_input, repo).map_err(error::Add::Usage)?,
    };

    // Resolve name: use -n if given, otherwise prompt with the path basename
    // as default (no default available when --cid was used without a path).
    let name = match name {
        Some(n) => n,
        None => {
            let default = path
                .as_deref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str());
            prompt::prompt_name(no_input, default).map_err(error::Add::Usage)?
        }
    };

    // One release per OID: reuse the existing release regardless of who
    // created it, otherwise create a fresh one. If duplicate release COBs
    // exist for this OID (concurrent creation across unsynced nodes), the
    // store deterministically picks one so all replicas converge on it.
    let mut release = releases
        .find_or_create_by_oid(oid)
        .map_err(|err| error::Add::Create { oid, err })?;
    let id = *release.id();
    release
        .add_artifact(cid, name.clone())
        .map_err(|err| error::Add::Store { id, err })?;
    let short_oid = &oid.to_string()[..7];
    eprintln!("Added artifact '{name}' to release {short_oid} ({id})");
    if std::io::stderr().is_terminal() {
        eprintln!("Hint: use `rad-artifact location add {short_oid} --cid {cid} <url>` to register a download location");
        if let Some(p) = path.as_deref() {
            eprintln!(
                "      or `rad-artifact serve {}` to seed it yourself over iroh",
                p.display()
            );
        }
    }
    println!("{cid}");
    Ok(())
}

/// Compute a CID by hashing the file or directory at `path`. Mirrors the
/// dispatch used by `serve` so both commands agree on what CID a given path
/// produces.
fn compute_cid_from_path(path: &std::path::Path) -> Result<Cid, error::Add> {
    if path.is_dir() {
        share::compute_content_id(path).map_err(error::Add::Io)
    } else {
        share::compute_blob_cid(path).map_err(error::Add::Protocol)
    }
}

fn location_add<Signer>(
    command::LocationAdd { commit, cid, url }: command::LocationAdd,
    releases: &mut Releases<Repository, WriteAs<Signer>>,
    repo: &Repository,
) -> Result<(), error::Locate>
where
    Signer: radicle::crypto::signature::Signer<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Signer<radicle::crypto::ssh::ExtendedSignature>,
    Signer: radicle::crypto::signature::Verifier<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Keypair<VerifyingKey = radicle::crypto::PublicKey>,
{
    let oid = resolve_commit(&commit, repo)?;
    let id = releases
        .find_unique_by_oid(oid)
        .map_err(error::Find::from)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Locate::Store { id, err })?;
    release
        .add_location(cid, url.clone())
        .map_err(|err| error::Locate::Store { id, err })?;
    eprintln!("Added location {url} for artifact {cid}");
    if std::io::stderr().is_terminal() {
        eprintln!("Hint: use `rad-artifact show --pretty {commit}` to verify the release");
    }
    Ok(())
}

fn attest_artifact<Signer>(
    command::Attest { commit, cid }: command::Attest,
    no_input: bool,
    releases: &mut Releases<Repository, WriteAs<Signer>>,
    repo: &Repository,
) -> Result<(), error::Attest>
where
    Signer: radicle::crypto::signature::Signer<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Signer<radicle::crypto::ssh::ExtendedSignature>,
    Signer: radicle::crypto::signature::Verifier<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Keypair<VerifyingKey = radicle::crypto::PublicKey>,
{
    let (oid, cid) = match (commit, cid) {
        (Some(commit), Some(cid)) => (resolve_commit(&commit, repo)?, cid),
        (None, None) => {
            prompt::pick_interactive(no_input, releases, repo).map_err(error::Attest::Usage)?
        }
        _ => unreachable!("clap enforces both-or-neither"),
    };
    // The COB state machine enforces per-signer ownership; no delegate filter needed.
    let id = releases
        .find_unique_by_oid(oid)
        .map_err(error::Find::from)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Attest::Store { id, err })?;
    release
        .attest(cid)
        .map_err(|err| error::Attest::Store { id, err })?;
    eprintln!("Attested artifact {cid}");
    Ok(())
}

fn redact_artifact<Signer>(
    command::Redact { 
      commit,
      cid,
      reason 
    }: command::Redact,
    no_input: bool,
    releases: &mut Releases<Repository, WriteAs<Signer>>,
    repo: &Repository,
) -> Result<(), error::Redact>
where
    Signer: radicle::crypto::signature::Signer<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Signer<radicle::crypto::ssh::ExtendedSignature>,
    Signer: radicle::crypto::signature::Verifier<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Keypair<VerifyingKey = radicle::crypto::PublicKey>,
{
    let (oid, cid, reason) = match (commit, cid) {
        (Some(commit), Some(cid)) => {
            let oid = resolve_commit(&commit, repo)?;
            let reason = match reason {
                Some(r) => r,
                None => prompt::prompt_reason(no_input).map_err(error::Redact::Usage)?,
            };
            (oid, cid, reason)
        }
        (None, None) => {
            let (oid, cid) =
                prompt::pick_interactive(no_input, releases, repo).map_err(error::Redact::Usage)?;
            let reason = match reason {
                Some(r) => r,
                None => prompt::prompt_reason(no_input).map_err(error::Redact::Usage)?,
            };
            (oid, cid, reason)
        }
        _ => unreachable!("clap enforces both-or-neither"),
    };
    // The COB state machine enforces per-signer ownership; no delegate filter needed.
    let id = releases
        .find_unique_by_oid(oid)
        .map_err(error::Find::from)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Redact::Store { id, err })?;
    release
        .redact(cid, reason)
        .map_err(|err| error::Redact::Artifact { id, err })?;
    eprintln!("Redacted artifact {cid}");
    Ok(())
}

fn location_remove<Signer>(
    command::LocationRemove { commit, cid, url }: command::LocationRemove,
    releases: &mut Releases<Repository, WriteAs<Signer>>,
    repo: &Repository,
) -> Result<(), error::RemoveLocation>
where
    Signer: radicle::crypto::signature::Signer<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Signer<radicle::crypto::ssh::ExtendedSignature>,
    Signer: radicle::crypto::signature::Verifier<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Keypair<VerifyingKey = radicle::crypto::PublicKey>,
{
    let oid = resolve_commit(&commit, repo)?;
    // Unlike location_add, we don't restrict to delegate-authored releases: any
    // user should be able to retract their own locations from any release, and
    // the COB state machine already enforces that only the original announcer
    // can remove a given location.
    let id = releases
        .find_unique_by_oid(oid)
        .map_err(error::Find::from)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::RemoveLocation::Store { id, err })?;
    release
        .remove_location(cid, url.clone())
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

fn show_release<Access: radicle::cob::store::access::Access>(
    command::Show {
        pretty,
        json,
        verbose,
        redacted,
        all_authors,
        commit,
    }: command::Show,
    releases: &Releases<Repository, Access>,
    repo: &Repository,
    delegates: &BTreeSet<Did>,
    aliases: &impl AliasStore,
) -> Result<(), error::Show> {
    let oid = resolve_commit(&commit, repo)?;
    let id = releases
        .find_unique_by_oid(oid)
        .map_err(error::Find::from)?;
    let release = releases
        .get(&id)
        .map_err(|err| error::Find::Lookup { oid, err })?
        .ok_or(error::Find::NoRelease(oid))?;
    let title = display::CommitTitle::title(repo, release.oid());
    let filters = display::Filters {
        delegates,
        redacted,
        all_authors,
    };
    let show =
        radicle_artifact::display::Release::new(id, &release, aliases, filters, title);
    if use_pretty(pretty, json) {
        println!("{}", show.pretty(verbose));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&show).map_err(error::Show::Json)?
        );
    }
    Ok(())
}

fn list_releases<Access: radicle::cob::store::access::Access>(
    command::List {
        pretty,
        json,
        verbose,
        all_authors,
        redacted,
        empty,
    }: command::List,
    releases: &Releases<Repository, Access>,
    repo: &Repository,
    aliases: &impl AliasStore,
) -> Result<(), error::List> {
    // Delegates drive both the redaction and author filters, so always
    // fetch them; the flags below bypass each filter independently.
    let delegates: BTreeSet<_> = repo
        .delegates()
        .map_err(error::List::Delegates)?
        .into_iter()
        .collect();
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
        });
    let filters = display::Filters {
        delegates: &delegates,
        redacted,
        all_authors,
    };
    let releases = display::Releases::new(iter, aliases, filters, empty, repo);
    if use_pretty(pretty, json) {
        println!("{}", releases.pretty(verbose));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&releases).map_err(error::List::Json)?
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Share commands
// ---------------------------------------------------------------------------

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

fn run_fetch<Access: radicle::cob::store::access::Access>(
    args: command::Fetch,
    no_input: bool,
    _profile: &Profile,
    releases: &Releases<Repository, Access>,
    repo: &Repository,
) -> Result<(), RadArtifactError> {
    // clap's `requires` ensures both or neither are provided.
    let (oid, cid) = match (args.commit, args.cid) {
        (Some(commit), Some(cid)) => {
            let oid = resolve_commit(&commit, repo)?;
            (oid, cid)
        }
        (None, None) => {
            prompt::pick_interactive(no_input, releases, repo).map_err(error::Share::Usage)?
        }
        _ => unreachable!("clap enforces both-or-neither"),
    };

    // Retrieval is CID-centric: the same artifact may appear in multiple
    // releases (duplicate release COBs for one OID, or an identical build
    // reused across different commits). We union locations across all of
    // them so the fetcher sees every known source.
    let matching = releases
        .find_by_cid(&cid)
        .map_err(|err| error::Share::Usage(err.to_string()))?;
    if matching.is_empty() {
        return Err(error::Share::ArtifactNotFound(cid).into());
    }
    // Sanity check: at least one release for the requested commit contains
    // this CID. Catches callers who pair a valid CID with the wrong OID.
    if !matching.iter().any(|(_, r)| r.oid() == &oid) {
        return Err(error::Share::Usage(format!(
            "artifact {cid} is not associated with commit {oid}"
        ))
        .into());
    }

    // Prefer the name/redactions view from a release that matches the
    // requested OID; fall back to any release containing the CID.
    let primary = matching
        .iter()
        .find(|(_, r)| r.oid() == &oid)
        .or_else(|| matching.first())
        .expect("matching is non-empty");
    let artifact = primary
        .1
        .artifact(&cid)
        .expect("find_by_cid guarantees this");

    // Aggregate redactions across all releases containing the CID so the
    // user sees every trusted-party warning, not just those on one release.
    let mut aggregated_redactions: std::collections::BTreeMap<Did, String> =
        std::collections::BTreeMap::new();
    for (_, release) in matching.iter() {
        if let Some(a) = release.artifact(&cid) {
            for (did, reason) in a.redactions() {
                aggregated_redactions
                    .entry(*did)
                    .or_insert_with(|| reason.clone());
            }
        }
    }
    if !aggregated_redactions.is_empty() {
        eprintln!("WARNING: this artifact has been redacted");
        for (did, reason) in aggregated_redactions.iter() {
            eprintln!("  {did}: {reason}");
        }
    }

    eprintln!("Artifact: {} (CID: {cid})", artifact.name());

    let locations = if let Some(ref url) = args.url {
        vec![share::Location::Url(url)]
    } else {
        let artifacts = matching.iter().filter_map(|(_, r)| r.artifact(&cid));
        artifact_locations(artifacts)?
    };
    // Short-circuit when no usable source exists
    if locations.is_empty() {
        return Err(error::Share::NoLocationsForCid { cid }.into());
    }
    eprintln!(
        "Trying {} location{}...",
        locations.len(),
        if locations.len() == 1 { "" } else { "s" }
    );

    let output_path = args.output.unwrap_or_else(|| {
        let name = artifact.name();
        std::path::PathBuf::from(format!("{}_{cid}", name.replace(' ', "_")))
    });

    let preset = share::EndpointPreset::from_env().map_err(error::Share::Protocol)?;
    let kind = share::artifact_kind(&cid).map_err(error::Share::Protocol)?;

    match kind {
        share::ArtifactKind::Blob => {
            share::download(&locations, &cid, &output_path, &preset)
                .map_err(error::Share::Protocol)?;
        }
        share::ArtifactKind::Collection => {
            share::download_collection(&locations, &cid, &output_path, &preset)
                .map_err(error::Share::Protocol)?;
        }
    }

    eprintln!("Saved to {}", output_path.display());
    Ok(())
}

fn run_serve<Signer>(
    args: command::Serve,
    no_input: bool,
    profile: &Profile,
    releases: &mut Releases<Repository, WriteAs<Signer>>,
) -> Result<(), RadArtifactError>
where 
    Signer: radicle::crypto::signature::Signer<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Signer<radicle::crypto::ssh::ExtendedSignature>,
    Signer: radicle::crypto::signature::Verifier<radicle::crypto::Signature>,
    Signer: radicle::crypto::signature::Keypair<VerifyingKey = radicle::crypto::PublicKey>,
{
    // Compute CID from the provided path.
    let cid = if args.path.is_dir() {
        share::compute_content_id(&args.path).map_err(error::Share::Io)?
    } else {
        share::compute_blob_cid(&args.path).map_err(error::Share::Protocol)?
    };

    // Register the serving location on a single release. Fetchers look up
    // by CID and union locations across every release that contains it, so
    // one registration is sufficient to make the server discoverable. Pick
    // the most recently created release as the most representative home
    // for the new location.
    let matching = releases
        .find_by_cid(&cid)
        .map_err(|e| error::Share::Usage(e.to_string()))?;
    let (release_id, release) = matching
        .into_iter()
        .max_by_key(|(_, r)| r.timestamp())
        .ok_or(error::Share::ArtifactNotFound(cid))?;
    let artifact = release.artifact(&cid).expect("find_by_cid guarantees this");
    let kind = share::artifact_kind(&cid).map_err(error::Share::Protocol)?;

    eprintln!("Artifact: {} (CID: {cid})", artifact.name());

    if !no_input && std::io::stdin().is_terminal() {
        let confirmed = inquire::Confirm::new("Add yourself as a location for this artifact?")
            .with_default(true)
            .prompt()
            .map_err(|e| error::Share::Usage(format!("confirmation cancelled: {e}")))?;
        if !confirmed {
            return Ok(());
        }
    }

    let passphrase = prompt::passphrase_for_keystore(&profile.keystore)?;
    let iroh_sk = share::radicle_secret_to_iroh(&profile.keystore, passphrase)
        .map_err(error::Share::Protocol)?;
    let preset = share::EndpointPreset::from_env().map_err(error::Share::Protocol)?;

    let rt = tokio::runtime::Runtime::new().map_err(error::Share::Io)?;
    rt.block_on(async {
        let server = share::Server::start(iroh_sk, preset)
            .await
            .map_err(error::Share::Protocol)?;

        match kind {
            share::ArtifactKind::Blob => {
                share::add_blob(server.store(), &args.path, &cid)
                    .await
                    .map_err(error::Share::Protocol)?;
            }
            share::ArtifactKind::Collection => {
                share::add_collection(server.store(), &args.path, &cid)
                    .await
                    .map_err(error::Share::Protocol)?;
            }
        }

        // Scheme-only marker; artifact_locations derives the iroh public key
        // from the DID that authored the location, not from the URL.
        let iroh_url = url::Url::parse("iroh://").expect("static URL is valid");

        {
            let mut release_mut = releases
                .get_mut(&release_id)
                .map_err(|e| error::Share::Usage(e.to_string()))?;
            release_mut
                .add_location(cid, iroh_url.clone())
                .map_err(|e| error::Share::Usage(e.to_string()))?;
        }

        eprintln!("Serving artifact via iroh-blobs");
        eprintln!("Press Ctrl+C to stop");

        tokio::signal::ctrl_c().await.map_err(error::Share::Io)?;

        eprintln!("\nShutting down...");

        // Retract the location we added. Surface failure as a warning so
        // shutdown still proceeds to the server teardown below.
        match releases.get_mut(&release_id) {
            Ok(mut release_mut) => {
                if let Err(e) = release_mut.remove_location(cid, iroh_url.clone()) {
                    eprintln!("Warning: failed to remove location from {release_id}: {e}");
                } else {
                    eprintln!("Removed location from release {release_id}");
                }
            }
            Err(e) => {
                eprintln!("Warning: failed to open release {release_id} for cleanup: {e}");
            }
        }

        server.shutdown().await.map_err(error::Share::Protocol)?;

        Ok::<_, RadArtifactError>(())
    })
}

/// Convert locations from one or more artifacts into fetch locations.
///
/// For `iroh://` URLs, derives the endpoint ID from the DID that authored the
/// location (same Ed25519 key). Locations are deduplicated across artifacts —
/// a plain URL contributed by the same or different users collapses to one
/// entry, and a DID's iroh endpoint collapses to one entry regardless of how
/// many releases record it.
fn artifact_locations<'a>(
    artifacts: impl IntoIterator<Item = &'a Artifact>,
) -> Result<Vec<share::Location<'a>>, RadArtifactError> {
    let mut seen_urls: BTreeSet<&url::Url> = BTreeSet::new();
    let mut seen_iroh: BTreeSet<Did> = BTreeSet::new();
    let mut locations = Vec::new();
    for artifact in artifacts {
        for (did, urls) in artifact.locations() {
            for url in urls {
                if url.scheme() == "iroh" {
                    if seen_iroh.insert(*did) {
                        let pk =
                            share::did_to_iroh_public_key(did).map_err(error::Share::Protocol)?;
                        locations.push(share::Location::Iroh(pk));
                    }
                } else if seen_urls.insert(url) {
                    locations.push(share::Location::Url(url));
                }
            }
        }
    }
    Ok(locations)
}

mod prompt {
    use std::io::IsTerminal;

    use radicle::git::Oid;
    use radicle::prelude::WriteRepository;
    use radicle::storage::git::Repository;

    use radicle_artifact::*;

    use super::display;

    /// Interactive mode: list commits with releases, pick one, list its
    /// artifacts, pick one.
    ///
    /// Releases are deduplicated by commit OID: if two release COBs exist
    /// for the same commit (e.g. concurrent creation before sync), their
    /// artifacts are merged into a single entry in the picker. Returns the
    /// chosen `(commit OID, CID)`; downstream callers look up by CID and
    /// union locations across all releases.
    ///
    /// Requires stdin to be a TTY. Errors if `no_input` is set or stdin is
    /// not interactive, so scripts don't hang waiting for input.
    pub fn pick_interactive<Access: radicle::cob::store::access::Access>(
        no_input: bool,
        releases: &Releases<Repository, Access>,
        repo: &Repository,
    ) -> Result<(Oid, radicle_artifact::Cid), String> {
        if no_input || !std::io::stdin().is_terminal() {
            return Err(
                "interactive mode requires a terminal; pass <commit> and --cid, or use --no-input to disable".into(),
            );
        }
        let all: Vec<Release> = releases
            .all()
            .map_err(|e| e.to_string())?
            .filter_map(|res| res.ok())
            .map(|(_, release)| release)
            .collect();

        if all.is_empty() {
            return Err("no releases found in this repository".into());
        }

        // Group releases by OID. Artifacts are merged (dedupe by CID) so
        // the picker shows one entry per commit even when multiple release
        // COBs exist for the same OID.
        let mut groups: std::collections::BTreeMap<Oid, CommitGroup> =
            std::collections::BTreeMap::new();
        for release in all.into_iter() {
            let oid = *release.oid();
            let ts = release.timestamp();
            let group = groups.entry(oid).or_insert_with(|| CommitGroup {
                oid,
                timestamp: ts,
                artifacts: Vec::new(),
            });
            // Track the earliest creation timestamp for this commit.
            group.timestamp = group.timestamp.min(ts);
            for (cid, artifact) in release.artifacts().iter() {
                if !group.artifacts.iter().any(|(c, _)| c == cid) {
                    group.artifacts.push((*cid, artifact.clone()));
                }
            }
        }
        let mut groups: Vec<CommitGroup> = groups.into_values().collect();
        // Most recently seen commit first.
        groups.sort_by_key(|g| std::cmp::Reverse(g.timestamp));

        let release_labels: Vec<String> = groups
            .iter()
            .map(|group| {
                let short = &group.oid.to_string()[..7];
                let title = display::CommitTitle::title(repo, &group.oid).unwrap_or_default();
                let n = group.artifacts.len();
                format!(
                    "{short} {title} ({n} artifact{})",
                    if n == 1 { "" } else { "s" }
                )
            })
            .collect();

        let selection = inquire::Select::new("Select release:", release_labels)
            .raw_prompt()
            .map_err(|e| format!("selection cancelled: {e}"))?;
        let group = &groups[selection.index];

        if group.artifacts.is_empty() {
            return Err("selected release has no artifacts".into());
        }

        let artifact_labels: Vec<String> = group
            .artifacts
            .iter()
            .map(|(cid, artifact)| {
                let redacted = if artifact.is_redacted() {
                    " [REDACTED]"
                } else {
                    ""
                };
                format!("{} (CID: {cid}){redacted}", artifact.name())
            })
            .collect();

        let selection = inquire::Select::new("Select artifact:", artifact_labels)
            .raw_prompt()
            .map_err(|e| format!("selection cancelled: {e}"))?;
        let (cid, _) = &group.artifacts[selection.index];

        Ok((group.oid, *cid))
    }

    /// Merged view of all release COBs for a given commit.
    struct CommitGroup {
        oid: Oid,
        timestamp: u64,
        artifacts: Vec<(radicle_artifact::Cid, Artifact)>,
    }

    /// Prompt for a redaction reason at the terminal.
    ///
    /// Errors if `no_input` is set or stdin is not a TTY.
    pub fn prompt_reason(no_input: bool) -> Result<String, String> {
        if no_input || !std::io::stdin().is_terminal() {
            return Err(
                "interactive mode requires a terminal; pass -m/--reason, or use --no-input to disable".into(),
            );
        }
        inquire::Text::new("Reason for redaction:")
            .prompt()
            .map_err(|e| format!("prompt cancelled: {e}"))
    }

    /// Prompt for an artifact name at the terminal.
    ///
    /// `default` is presented as a pre-filled value when provided (typically
    /// the basename of the artifact path). Errors if `no_input` is set or
    /// stdin is not a TTY, so scripts don't hang.
    pub fn prompt_name(no_input: bool, default: Option<&str>) -> Result<String, String> {
        if no_input || !std::io::stdin().is_terminal() {
            return Err("pass -n/--name for non-interactive use".into());
        }
        let mut text = inquire::Text::new("Artifact name:");
        if let Some(d) = default {
            text = text.with_default(d);
        }
        text.prompt().map_err(|e| format!("prompt cancelled: {e}"))
    }

    /// Interactively pick a commit or annotated tag from the repository.
    ///
    /// Presents annotated tags (peeled to their target commit) first,
    /// followed by recent commits reachable from HEAD, up to
    /// [`PICKER_COMMIT_LIMIT`] total commit entries. Commits already covered
    /// by an annotated tag entry are skipped. Errors if `no_input` is set or
    /// stdin is not a TTY.
    pub fn pick_commit_or_tag(no_input: bool, repo: &Repository) -> Result<Oid, String> {
        if no_input || !std::io::stdin().is_terminal() {
            return Err("pass --commit <REF> for non-interactive use".into());
        }
        let raw = repo.raw();
        let mut entries: Vec<Entry> = Vec::new();
        let mut seen: std::collections::BTreeSet<Oid> = std::collections::BTreeSet::new();

        // Annotated tags first — they're the common "pick a release" case.
        // Lightweight tags are intentionally skipped: their ref points
        // directly at a commit already covered by the HEAD walk below.
        // Sort by the peeled commit's committer time (newest first) so
        // releases read top-down instead of alphabetically, where e.g.
        // `v0.10.0` would otherwise appear before `v0.9.0`.
        let tag_names = raw
            .tag_names(None)
            .map_err(|e| format!("failed to list tags: {e}"))?;
        let mut tag_entries: Vec<(i64, String, Oid)> = Vec::new();
        for maybe_name in tag_names.iter() {
            let Some(name) = maybe_name else { continue };
            let full = format!("refs/tags/{name}");
            let Ok(reference) = raw.find_reference(&full) else {
                continue;
            };
            let Some(ref_target) = reference.target() else {
                continue;
            };
            let Ok(obj) = raw.find_object(ref_target, None) else {
                continue;
            };
            if obj.kind() != Some(radicle::git::raw::ObjectType::Tag) {
                continue;
            }
            let Ok(peeled) = reference.peel(radicle::git::raw::ObjectType::Commit) else {
                continue;
            };
            let commit_oid: Oid = peeled.id().into();
            let time = peeled.as_commit().map(|c| c.time().seconds()).unwrap_or(0);
            tag_entries.push((time, name.to_string(), commit_oid));
        }
        tag_entries.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, name, commit_oid) in tag_entries {
            if seen.insert(commit_oid) {
                entries.push(Entry::Tag {
                    name,
                    oid: commit_oid,
                });
            }
        }

        // Recent commits walked from HEAD, dedup against tag targets. The
        // default revwalk order visits each commit before its parents, i.e.
        // reverse chronological on a linear history — good enough for a
        // picker cap at `PICKER_COMMIT_LIMIT`.
        if let Ok(mut revwalk) = raw.revwalk() {
            if revwalk.push_head().is_ok() {
                let mut added = 0usize;
                for oid_res in revwalk {
                    if added >= PICKER_COMMIT_LIMIT {
                        break;
                    }
                    let Ok(oid) = oid_res else { continue };
                    let commit_oid: Oid = oid.into();
                    if seen.insert(commit_oid) {
                        entries.push(Entry::Commit { oid: commit_oid });
                        added += 1;
                    }
                }
            }
        }

        if entries.is_empty() {
            return Err("no commits or annotated tags found in this repository".into());
        }

        let labels: Vec<String> = entries
            .iter()
            .map(|entry| {
                let oid = entry.oid();
                let short = &oid.to_string()[..7];
                let title = display::CommitTitle::title(repo, &oid).unwrap_or_default();
                match entry {
                    Entry::Tag { name, .. } => format!("{name} -> {short}  {title}"),
                    Entry::Commit { .. } => format!("{short}  {title}"),
                }
            })
            .collect();

        let selection = inquire::Select::new("Select commit or tag:", labels)
            .raw_prompt()
            .map_err(|e| format!("selection cancelled: {e}"))?;
        Ok(entries[selection.index].oid())
    }

    /// Cap on commits shown in the picker — enough to cover typical recent
    /// activity without overwhelming the terminal UI.
    const PICKER_COMMIT_LIMIT: usize = 30;

    enum Entry {
        Tag { name: String, oid: Oid },
        Commit { oid: Oid },
    }

    impl Entry {
        fn oid(&self) -> Oid {
            match self {
                Entry::Tag { oid, .. } | Entry::Commit { oid } => *oid,
            }
        }
    }

    pub fn passphrase_for_keystore(
        keystore: &radicle::crypto::ssh::keystore::Keystore,
    ) -> Result<Option<radicle::crypto::ssh::keystore::Passphrase>, super::error::Share> {
        use radicle::crypto::ssh::keystore::Passphrase;

        let is_encrypted = keystore
            .is_encrypted()
            .map_err(|e| super::error::Share::Usage(format!("failed to check keystore: {e}")))?;

        if !is_encrypted {
            return Ok(None);
        }

        // Try env var first, matching radicle convention.
        if let Some(passphrase) = radicle::profile::env::passphrase() {
            return Ok(Some(passphrase));
        }

        if !std::io::stderr().is_terminal() {
            return Err(super::error::Share::Usage(
                "encrypted keystore requires RAD_PASSPHRASE (no terminal for prompt)".into(),
            ));
        }

        let passphrase = inquire::Password::new("Enter passphrase to unlock your radicle key:")
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .without_confirmation()
            .prompt()
            .map_err(|e| super::error::Share::Usage(format!("passphrase prompt failed: {e}")))?;

        Ok(Some(Passphrase::from(passphrase)))
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
    #[error(transparent)]
    Delegates(#[from] error::Delegates),
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
        #[clap(name = "cid")]
        ComputeCid(ComputeCid),
        /// Fetch an artifact from a release COB
        Fetch(Fetch),
        /// Serve an artifact via iroh-blobs using your radicle identity
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
    /// Computes the CID from the given path, looks up the matching artifact
    /// in existing releases, registers an `iroh://` location in the release
    /// COB, and serves the content until interrupted. The location is removed
    /// on graceful shutdown (Ctrl+C).
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Serve an artifact:
    $ rad-artifact serve ./my-binary")]
    pub struct Serve {
        /// Path to file or directory to serve.
        pub path: std::path::PathBuf,
    }

    /// Add an artifact to a release, creating the release if needed.
    ///
    /// The artifact is identified by a content identifier (CID). Pass a
    /// local <PATH> to compute the CID from the file or directory
    /// contents, or use --cid to register a precomputed CID for an
    /// artifact you don't have locally. Exactly one of <PATH> or --cid
    /// must be provided.
    ///
    /// The release commit and artifact name are prompted interactively
    /// when not given. Pass --commit and -n/--name to skip prompts (or
    /// use --no-input in scripts to fail instead of hanging on a prompt).
    #[derive(Parser)]
    #[clap(
        group = clap::ArgGroup::new("source").required(true).args(["path", "cid"]),
        after_long_help = "\
Examples:
  Interactive: compute CID from a file, pick commit/tag, prompt for name:
    $ rad-artifact add ./my-binary

  Fully non-interactive:
    $ rad-artifact add ./my-binary --commit abc1234 --name \"my-binary v1.0\"

  Register a precomputed CID without local bytes:
    $ rad-artifact add --cid baf...abc --commit abc1234 --name \"my-binary v1.0\""
    )]
    pub struct Add {
        /// Path to the local file or directory to register.
        ///
        /// The CID is computed from the contents: files use the raw codec
        /// (0x55), directories use the blake3-hashseq codec (0x80).
        pub path: Option<std::path::PathBuf>,
        /// Precomputed CID. Use when the artifact bytes aren't available
        /// locally. Conflicts with <PATH>.
        #[clap(long)]
        pub cid: Option<Cid>,
        /// Git commit, tag, or abbreviated OID of the release. Prompts
        /// interactively when omitted.
        #[clap(long)]
        pub commit: Option<String>,
        /// Human-readable name for the artifact. Prompts interactively
        /// when omitted (with the path basename as the default).
        #[clap(short, long)]
        pub name: Option<String>,
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
    ///
    /// Without arguments, interactively lists releases and artifacts to
    /// pick from. Pass both <COMMIT> and --cid to skip the prompts.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact attest

  Attest a specific artifact:
    $ rad-artifact attest abc1234 --cid baf...abc")]
    pub struct Attest {
        /// Git commit, tag, or abbreviated OID of the release. Required with --cid.
        #[clap(requires = "cid")]
        pub commit: Option<String>,
        /// Content identifier for the artifact to attest. Required with <COMMIT>.
        #[clap(long, requires = "commit")]
        pub cid: Option<Cid>,
    }

    /// Redact an artifact, indicating it should not be used.
    ///
    /// Records that the signing node believes this artifact is compromised
    /// or should be withdrawn. The reason is a free-form string (max 2048
    /// bytes). The act of redaction is permanent; the reason text can be
    /// amended by redacting again. A redaction supersedes any prior
    /// attestation from the same DID.
    ///
    /// Without arguments, interactively lists releases and artifacts to
    /// pick from and prompts for a reason. Pass both <COMMIT> and --cid
    /// to skip the release/artifact prompts; -m is still optional and
    /// will be prompted if omitted at a terminal.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact redact

  Redact a specific artifact:
    $ rad-artifact redact abc1234 --cid baf...abc -m \"build compromised, see advisory\"")]
    pub struct Redact {
        /// Git commit, tag, or abbreviated OID of the release. Required with --cid.
        #[clap(requires = "cid")]
        pub commit: Option<String>,
        /// Content identifier for the artifact to redact. Required with <COMMIT>.
        #[clap(long, requires = "commit")]
        pub cid: Option<Cid>,
        /// Reason for the redaction.
        #[clap(short = 'm', long = "reason")]
        pub reason: Option<String>,
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
    ///
    /// By default only artifacts authored by a repository delegate are
    /// shown. Pass `--all-authors` to include artifacts added by other users.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Show a release as JSON (default):
    $ rad-artifact show abc1234

  Show a release in human-readable format:
    $ rad-artifact show --pretty abc1234

  Include redacted artifacts and artifacts from non-delegates:
    $ rad-artifact show --pretty --redacted --all-authors abc1234")]
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
        /// Output all information, including intermediate errors.
        #[clap(long, short)]
        pub verbose: bool,
        /// Also show artifacts that have been redacted by a trusted party.
        #[clap(long)]
        pub redacted: bool,
        /// Also include artifacts authored by users who are not
        /// repository delegates.
        #[clap(long)]
        pub all_authors: bool,
        /// Git commit, tag, or abbreviated OID of the release.
        pub commit: String,
    }

    /// List all release COBs for a repository.
    ///
    /// By default only artifacts authored by a repository delegate are
    /// shown. Pass `--all-authors` to include artifacts added by other users.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  List releases with delegate-authored artifacts as JSON:
    $ rad-artifact list

  Include artifacts from non-delegate authors:
    $ rad-artifact list --pretty --all-authors

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
        /// Also include artifacts authored by users who are not
        /// repository delegates.
        #[clap(long)]
        pub all_authors: bool,
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
        #[error("{0}")]
        Usage(String),
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("failed to create release for commit {oid}")]
        Create {
            oid: Oid,
            #[source]
            err: cob::store::Error,
        },
        #[error("failed to add artifact to release {id}")]
        Store {
            id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
        #[error("failed to compute CID from path")]
        Io(#[source] std::io::Error),
        #[error(transparent)]
        Protocol(radicle_artifact::share::Error),
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
        #[error("{0}")]
        Usage(String),
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
        #[error("{0}")]
        Usage(String),
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("failed to redact artifact in release {id}")]
        Artifact {
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
        #[error("multiple delegate releases found for the commit {0}")]
        Ambiguous(Oid),
        #[error("failed to find a release for the commit {oid}")]
        Lookup {
            oid: Oid,
            #[source]
            err: cob::store::Error,
        },
    }

    impl From<radicle_artifact::error::FindRelease> for Find {
        fn from(e: radicle_artifact::error::FindRelease) -> Self {
            match e {
                radicle_artifact::error::FindRelease::NoRelease(oid) => Self::NoRelease(oid),
                radicle_artifact::error::FindRelease::Ambiguous(oid) => Self::Ambiguous(oid),
                radicle_artifact::error::FindRelease::Store { oid, err } => {
                    Self::Lookup { oid, err }
                }
            }
        }
    }

    #[derive(Debug, Error)]
    #[error("could not resolve '{commit}' to a git object")]
    pub struct Resolve {
        pub commit: String,
        #[source]
        pub err: radicle::git::raw::Error,
    }

    #[derive(Debug, Error)]
    #[error("failed to get repository delegates")]
    pub struct Delegates(#[source] pub RepositoryError);

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

    #[derive(Debug, Error)]
    pub enum Share {
        #[error("{0}")]
        Usage(String),
        #[error("artifact with CID {0} not found")]
        ArtifactNotFound(radicle_artifact::Cid),
        // Distinct from `ArtifactNotFound`: the artifact is known, but no
        // usable source has been announced. Surface the actionable recovery
        // paths so the user doesn't get a generic "no locations" error.
        #[error("no download locations known for artifact {cid}\n  hint: pass --url <URL> to fetch directly, or ask a seed to run `rad-artifact serve`")]
        NoLocationsForCid { cid: radicle_artifact::Cid },
        #[error(transparent)]
        Protocol(radicle_artifact::share::Error),
        #[error("I/O error")]
        Io(#[source] std::io::Error),
    }
}
