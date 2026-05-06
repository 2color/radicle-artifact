//! A cli to create and inspect artifact release COBs for a repository.
//!
//! Run `rad-artifact --help` to see how to use the cli.

use std::{collections::BTreeSet, error::Error as _, io::IsTerminal, time::Duration};

use clap::Parser;

use radicle::{
    cob, crypto,
    crypto::signature::Signer,
    git::Oid,
    identity::Did,
    node::{
        device::Device,
        sync::{Announcer, AnnouncerConfig, ReplicationFactor},
        AliasStore, Handle, Node,
    },
    prelude::{Profile, ReadRepository, ReadStorage, RepoId, WriteRepository},
    profile,
    storage::git::Repository,
};
use radicle_artifact::share;
use radicle_artifact::*;
use url::Url;

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

fn repo_delegates(repo: &Repository) -> Result<BTreeSet<Did>, error::Delegates> {
    Ok(repo
        .delegates()
        .map_err(error::Delegates)?
        .into_iter()
        .collect())
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
    if let Command::ComputeCid(cmd) = args.command {
        return run_cid(cmd);
    }

    let profile = load_profile()?;
    let repo = args.repository(&profile)?;
    let mut releases = open_releases(&repo)?;
    match args.command {
        Command::ComputeCid(_) => unreachable!(), // handled above
        Command::Add(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            add_artifact(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Location(loc) => {
            let signer = profile.signer().map_err(error::Signer)?;
            match loc.command {
                LocationCommand::Add(cmd) => {
                    location_add(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
                }
                LocationCommand::Remove(cmd) => {
                    location_remove(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
                }
            }
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Attest(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            attest_artifact(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Redact(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            redact_artifact(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Show(cmd) => {
            let delegates = repo_delegates(&repo)?;
            let local = Did::from(*profile.id());
            show_release(cmd, &releases, &repo, &delegates, &local, &profile)?;
        }
        Command::List(cmd) => {
            let local = Did::from(*profile.id());
            list_releases(cmd, &releases, &repo, &local, &profile)?;
        }
        Command::Fetch(cmd) => run_fetch(cmd, args.no_input, &profile, &releases, &repo)?,
        Command::Serve(cmd) => run_serve(cmd, args.no_input, &profile, &mut releases)?,
    }

    Ok(())
}

fn add_artifact<G>(
    command::Add {
        path,
        cid,
        revision,
        release,
        name,
    }: command::Add,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    aliases: &impl AliasStore,
    signer: &Device<G>,
) -> Result<(), error::Add>
where
    G: Signer<crypto::Signature>,
{
    // ArgGroup guarantees exactly one of path/cid, but surface a usage
    // error rather than panic if clap ever changes its mind.
    let cid = match (path.as_deref(), cid) {
        (Some(path), None) => compute_cid_from_path(path)?,
        (None, Some(cid)) => cid,
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

    let (mut release, oid) = match release.as_deref() {
        Some(s) => {
            let release_id = parse_release_id(s, repo)?;
            let release = releases.get_mut(&release_id).map_err(|err| match err {
                cob::store::Error::NotFound(_, _) => error::Find::NoReleaseId(release_id).into(),
                err => error::Add::Find(error::Find::LookupId { release_id, err }),
            })?;
            let oid = *release.oid();
            (release, oid)
        }
        None => {
            let resolved = match revision.as_deref() {
                Some(rev) => resolve_ref(rev, repo)?,
                None => prompt::pick_commit_or_tag(no_input, repo).map_err(error::Add::Usage)?,
            };
            let oid = resolved.commit;
            let candidates: Vec<(ReleaseId, Release)> = releases
                .find_by_commit(oid)
                .map_err(|err| error::Find::Lookup { oid, err })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| error::Find::Lookup { oid, err })?;

            // Disambiguate when multiple releases exist OR when a single
            // existing release would silently inherit the wrong tag (e.g.
            // an RC release on the same commit, the user supplies the
            // final tag). When the user supplied no tag, silently reuse
            // the existing release — they didn't assert anything to
            // contradict.
            let needs_disambiguation = candidates.len() > 1
                || (candidates.len() == 1
                    && resolved.tag.is_some()
                    && candidates[0].1.tag().copied() != resolved.tag);

            let release = if candidates.is_empty() {
                releases
                    .create(oid, resolved.tag, signer)
                    .map_err(|err| error::Add::Create { oid, err })?
            } else if !needs_disambiguation {
                let id = candidates[0].0;
                releases
                    .get_mut(&id)
                    .map_err(|err| error::Add::Store { id, err })?
            } else if no_input {
                return Err(error::Add::NeedsDisambiguation {
                    oid,
                    candidates: candidates.iter().map(|(id, _)| *id).collect(),
                });
            } else {
                match prompt::pick_release_or_create(&candidates, resolved.tag, repo, aliases)
                    .map_err(error::Add::Usage)?
                {
                    prompt::ReleaseChoice::Existing(id) => releases
                        .get_mut(&id)
                        .map_err(|err| error::Add::Store { id, err })?,
                    prompt::ReleaseChoice::CreateNew => releases
                        .create(oid, resolved.tag, signer)
                        .map_err(|err| error::Add::Create { oid, err })?,
                }
            };
            (release, oid)
        }
    };
    let id = *release.id();
    release
        .add_artifact(cid, name.clone(), signer)
        .map_err(|err| error::Add::Store { id, err })?;
    let short_oid = &oid.to_string()[..7];
    let short_id = &id.to_string()[..7];
    eprintln!("Added artifact '{name}' to release {short_id} (commit {short_oid})");
    if std::io::stderr().is_terminal() {
        eprintln!("Hint: use `rad-artifact location add --release {short_id} --cid {cid} <url>` to register a download location");
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

fn location_add<G>(
    command::LocationAdd {
        revision,
        release,
        cid,
        url,
    }: command::LocationAdd,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &Device<G>,
) -> Result<(), error::Locate>
where
    G: Signer<crypto::Signature>,
{
    let delegates = repo_delegates(repo)?;
    let release_arg = release.as_deref();
    let revision_arg = revision.as_deref();
    let (id, cid) = match (release_arg, revision_arg, cid) {
        (Some(_), _, Some(cid)) | (_, Some(_), Some(cid)) => {
            let id = resolve_target_release(
                release_arg,
                revision_arg,
                releases,
                repo,
                &delegates,
                no_input,
                profile,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) =
                prompt::pick_interactive(no_input, releases, repo).map_err(error::Locate::Usage)?;
            let id = releases
                .find_unique_by_commit(oid, &delegates)
                .map_err(error::Find::from)?;
            (id, cid)
        }
        _ => unreachable!("clap enforces a target arg with --cid and vice versa"),
    };
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Locate::Store { id, err })?;
    release
        .add_location(cid, url.clone(), signer)
        .map_err(|err| error::Locate::Store { id, err })?;
    eprintln!("Added location {url} for artifact {cid}");
    if std::io::stderr().is_terminal() {
        eprintln!("Hint: use `rad-artifact show --pretty --release {id}` to verify the release");
    }
    Ok(())
}

fn attest_artifact<G>(
    command::Attest {
        revision,
        release,
        cid,
    }: command::Attest,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    aliases: &impl AliasStore,
    signer: &Device<G>,
) -> Result<(), error::Attest>
where
    G: Signer<crypto::Signature>,
{
    let delegates = repo_delegates(repo)?;
    let release = release.as_deref();
    let revision = revision.as_deref();
    let (id, cid) = match (release, revision, cid) {
        (Some(_), _, Some(cid)) | (_, Some(_), Some(cid)) => {
            let id = resolve_target_release(
                release, revision, releases, repo, &delegates, no_input, aliases,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) =
                prompt::pick_interactive(no_input, releases, repo).map_err(error::Attest::Usage)?;
            let id = releases
                .find_unique_by_commit(oid, &delegates)
                .map_err(error::Find::from)?;
            (id, cid)
        }
        _ => unreachable!("clap enforces a target arg with --cid and vice versa"),
    };
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
    command::Redact {
        revision,
        release,
        cid,
        reason,
    }: command::Redact,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    aliases: &impl AliasStore,
    signer: &Device<G>,
) -> Result<(), error::Redact>
where
    G: Signer<crypto::Signature>,
{
    let delegates = repo_delegates(repo)?;
    let release = release.as_deref();
    let revision = revision.as_deref();
    let (id, cid) = match (release, revision, cid) {
        (Some(_), _, Some(cid)) | (_, Some(_), Some(cid)) => {
            let id = resolve_target_release(
                release, revision, releases, repo, &delegates, no_input, aliases,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) =
                prompt::pick_interactive(no_input, releases, repo).map_err(error::Redact::Usage)?;
            let id = releases
                .find_unique_by_commit(oid, &delegates)
                .map_err(error::Find::from)?;
            (id, cid)
        }
        _ => unreachable!("clap enforces a target arg with --cid and vice versa"),
    };
    let reason = match reason {
        Some(r) => r,
        None => prompt::prompt_reason(no_input).map_err(error::Redact::Usage)?,
    };
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Redact::Store { id, err })?;
    release
        .redact(cid, reason, signer)
        .map_err(|err| error::Redact::Artifact { id, err })?;
    eprintln!("Redacted artifact {cid}");
    Ok(())
}

fn location_remove<G>(
    command::LocationRemove {
        revision,
        release,
        cid,
        url,
    }: command::LocationRemove,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &Device<G>,
) -> Result<(), error::RemoveLocation>
where
    G: Signer<crypto::Signature>,
{
    let delegates = repo_delegates(repo)?;
    let release_arg = release.as_deref();
    let revision_arg = revision.as_deref();
    let (id, cid) = match (release_arg, revision_arg, cid) {
        (Some(_), _, Some(cid)) | (_, Some(_), Some(cid)) => {
            let id = resolve_target_release(
                release_arg,
                revision_arg,
                releases,
                repo,
                &delegates,
                no_input,
                profile,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) = prompt::pick_interactive(no_input, releases, repo)
                .map_err(error::RemoveLocation::Usage)?;
            let id = releases
                .find_unique_by_commit(oid, &delegates)
                .map_err(error::Find::from)?;
            (id, cid)
        }
        _ => unreachable!("clap enforces a target arg with --cid and vice versa"),
    };

    // Look up the user's own registered locations for this artifact so we
    // can either pick from them (when URL is omitted) or verify the URL the
    // user supplied. Without this check `remove_location` silently no-ops
    // for unknown URLs / wrong-DID retractions (see lib.rs test
    // `remove_location_for_node_that_never_added_is_noop`).
    let local = Did::from(*profile.id());
    let urls: Vec<Url> = {
        let r = releases
            .get(&id)
            .map_err(|err| error::RemoveLocation::Store { id, err })?
            .ok_or_else(|| error::RemoveLocation::Usage(format!("release {id} not found")))?;
        let a = r.artifact(&cid).ok_or_else(|| {
            error::RemoveLocation::Usage(format!("no artifact {cid} in release {id}"))
        })?;
        a.locations()
            .get(&local)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    };
    let url = match url {
        Some(url) => {
            if !urls.contains(&url) {
                return Err(error::RemoveLocation::Usage(format!(
                    "you have not registered location {url} for artifact {cid}"
                )));
            }
            url
        }
        None => prompt::pick_location(no_input, urls).map_err(error::RemoveLocation::Usage)?,
    };

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

/// Build a [`display::Style`]: color is on when stdout is a TTY and `NO_COLOR`
/// is unset, off otherwise. `verbose` is forwarded to the style.
fn pretty_style(verbose: bool) -> display::Style {
    let color = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    display::Style { verbose, color }
}

fn show_release(
    command::Show {
        pretty,
        json,
        verbose,
        redacted,
        all_authors,
        revision,
        release,
    }: command::Show,
    releases: &Releases<Repository>,
    repo: &Repository,
    delegates: &BTreeSet<Did>,
    local: &Did,
    aliases: &impl AliasStore,
) -> Result<(), error::Show> {
    // --release narrows to one release; <revision> returns every
    // release for the resolved commit. JSON always emits an array —
    // single-element when --release — so consumers don't branch on
    // input shape.
    let candidates: Vec<(ReleaseId, radicle_artifact::Release)> =
        match (release.as_deref(), revision.as_deref()) {
            (Some(s), _) => {
                let id = parse_release_id(s, repo)?;
                let r = releases
                    .get(&id)
                    .map_err(|err| error::Find::LookupId {
                        release_id: id,
                        err,
                    })?
                    .ok_or(error::Find::NoReleaseId(id))?;
                vec![(id, r)]
            }
            (None, Some(rev)) => {
                let oid = resolve_ref(rev, repo)?.commit;
                let hits: Vec<_> = releases
                    .find_by_commit(oid)
                    .map_err(|err| error::Find::Lookup { oid, err })?
                    .collect::<Result<_, _>>()
                    .map_err(|err| error::Find::Lookup { oid, err })?;
                if hits.is_empty() {
                    return Err(error::Find::NoRelease(oid).into());
                }
                hits
            }
            (None, None) => unreachable!("clap requires one of --release or <revision>"),
        };

    let filters = display::Filters {
        delegates,
        redacted,
        all_authors,
        local: Some(local),
    };
    let shown = display::Releases::new(candidates.into_iter(), aliases, filters, true, repo, repo);
    if use_pretty(pretty, json) {
        print!("{}", shown.pretty_detailed(pretty_style(verbose)));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&shown.into_inner()).map_err(error::Show::Json)?
        );
    }
    Ok(())
}

fn list_releases(
    command::List {
        pretty,
        json,
        verbose,
        all_authors,
        redacted,
        empty,
    }: command::List,
    releases: &Releases<Repository>,
    repo: &Repository,
    local: &Did,
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
        local: Some(local),
    };
    let releases = display::Releases::new(iter, aliases, filters, empty, repo, repo);
    if use_pretty(pretty, json) {
        print!("{}", releases.pretty(pretty_style(verbose)));
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

fn run_fetch(
    args: command::Fetch,
    no_input: bool,
    _profile: &Profile,
    releases: &Releases<Repository>,
    repo: &Repository,
) -> Result<(), RadArtifactError> {
    // clap's `requires` ensures both or neither are provided.
    let (oid, cid) = match (args.revision, args.cid) {
        (Some(revision), Some(cid)) => {
            let oid = resolve_ref(&revision, repo)?.commit;
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

fn run_serve(
    args: command::Serve,
    no_input: bool,
    profile: &Profile,
    releases: &mut Releases<Repository>,
) -> Result<(), RadArtifactError> {
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

        let signer = profile.signer().map_err(error::Signer)?;
        {
            let mut release_mut = releases
                .get_mut(&release_id)
                .map_err(|e| error::Share::Usage(e.to_string()))?;
            release_mut
                .add_location(cid, iroh_url.clone(), &signer)
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
                if let Err(e) = release_mut.remove_location(cid, iroh_url.clone(), &signer) {
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
    use radicle::node::AliasStore;
    use radicle::prelude::WriteRepository;
    use radicle::storage::git::Repository;

    use radicle_artifact::*;
    use url::Url;

    use super::display;

    /// User's choice from the multi-release picker: reuse one of the
    /// existing releases, or create a brand-new release for the commit.
    pub enum ReleaseChoice {
        Existing(ReleaseId),
        CreateNew,
    }

    /// Prompt the user to disambiguate among multiple releases for the
    /// same commit, or to create a new one. `new_tag = Some` appends a
    /// "Create new release (tag X)" entry; `None` omits it. The tag
    /// short OID surfaces the RC-promotion case (RC tag → final tag on
    /// same commit) when present.
    pub fn pick_release_or_create(
        candidates: &[(ReleaseId, Release)],
        new_tag: Option<Oid>,
        repo: &Repository,
        aliases: &impl AliasStore,
    ) -> Result<ReleaseChoice, String> {
        let create_label = new_tag.map(|tag| {
            let short = &tag.to_string()[..7];
            format!("Create new release (tag {short})")
        });
        match select_release(candidates, create_label, repo, aliases)? {
            Some(id) => Ok(ReleaseChoice::Existing(id)),
            None => Ok(ReleaseChoice::CreateNew),
        }
    }

    /// Prompt the user to pick one release among several that exist
    /// for the same commit. Used by read/modify commands when a
    /// revision lookup is ambiguous; no "create new" option since
    /// these commands act on existing releases.
    pub fn pick_existing_release(
        candidates: &[(ReleaseId, Release)],
        repo: &Repository,
        aliases: &impl AliasStore,
    ) -> Result<ReleaseId, String> {
        select_release(candidates, None, repo, aliases)?.ok_or_else(|| "no release selected".into())
    }

    /// Show a multi-release picker. Returns `Some(id)` when the user
    /// picked a candidate, or `None` when they picked the optional
    /// `extra_label` entry (used by callers that offer "create new").
    fn select_release(
        candidates: &[(ReleaseId, Release)],
        extra_label: Option<String>,
        repo: &Repository,
        aliases: &impl AliasStore,
    ) -> Result<Option<ReleaseId>, String> {
        if !std::io::stdin().is_terminal() {
            return Err(
                "multiple releases exist for this commit; pass --release <id> to disambiguate"
                    .into(),
            );
        }
        let mut labels: Vec<String> = candidates
            .iter()
            .map(|(id, release)| format_candidate(id, release, repo, aliases))
            .collect();
        if let Some(extra) = extra_label {
            labels.push(extra);
        }
        let selection = inquire::Select::new("Select release:", labels)
            .raw_prompt()
            .map_err(|e| format!("selection cancelled: {e}"))?;
        if selection.index < candidates.len() {
            Ok(Some(candidates[selection.index].0))
        } else {
            Ok(None)
        }
    }

    fn format_candidate(
        id: &ReleaseId,
        release: &Release,
        repo: &Repository,
        aliases: &impl AliasStore,
    ) -> String {
        let id_str = id.to_string();
        let short_id = &id_str[..7];
        let tag = match release.tag() {
            Some(tag) => format!("tag {}", &tag.to_string()[..7]),
            None => "no tag".to_string(),
        };
        let creator = display::format_did(
            release.creator(),
            &display::resolve(release.creator(), aliases),
            false,
        );
        let title = display::CommitTitle::title(repo, release.oid()).unwrap_or_default();
        format!("{short_id}  {tag}  by {creator}  {title}")
    }

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
    pub fn pick_interactive(
        no_input: bool,
        releases: &Releases<Repository>,
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

    /// Pick a previously-announced location URL from a list.
    ///
    /// Used by `location remove` to let the user choose which of their
    /// own registered locations to retract. Errors if `no_input` is set,
    /// stdin is not a TTY, or the list is empty.
    pub fn pick_location(no_input: bool, urls: Vec<Url>) -> Result<Url, String> {
        if no_input || !std::io::stdin().is_terminal() {
            return Err(
                "interactive mode requires a terminal; pass <URL>, or use --no-input to disable"
                    .into(),
            );
        }
        if urls.is_empty() {
            return Err("no locations registered by you for this artifact".into());
        }
        let labels: Vec<String> = urls.iter().map(|u| u.to_string()).collect();
        let selection = inquire::Select::new("Select location to remove:", labels)
            .raw_prompt()
            .map_err(|e| format!("selection cancelled: {e}"))?;
        Ok(urls[selection.index].clone())
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
    /// Annotated tags appear first, followed by up to
    /// [`PICKER_COMMIT_LIMIT`] recent commits reachable from HEAD. A
    /// tag and its peeled commit both appear as separate entries so
    /// the user can opt out of tag-association by picking the commit.
    /// Errors if `no_input` is set or stdin is not a TTY.
    pub fn pick_commit_or_tag(
        no_input: bool,
        repo: &Repository,
    ) -> Result<super::ResolvedRef, String> {
        if no_input || !std::io::stdin().is_terminal() {
            return Err("pass --commit <REF> for non-interactive use".into());
        }
        let raw = repo.raw();
        let mut entries: Vec<Entry> = Vec::new();

        // Annotated tags first — they're the common "pick a release" case.
        // Lightweight tags are intentionally skipped: their ref points
        // directly at a commit already covered by the HEAD walk below.
        // Sort by the peeled commit's committer time (newest first) so
        // releases read top-down instead of alphabetically, where e.g.
        // `v0.10.0` would otherwise appear before `v0.9.0`.
        let tag_names = raw
            .tag_names(None)
            .map_err(|e| format!("failed to list tags: {e}"))?;
        // (committer time, tag name, tag object OID, peeled commit OID).
        let mut tag_entries: Vec<(i64, String, Oid, Oid)> = Vec::new();
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
            let tag_oid: Oid = ref_target.into();
            let Ok(peeled) = reference.peel(radicle::git::raw::ObjectType::Commit) else {
                continue;
            };
            let commit_oid: Oid = peeled.id().into();
            let time = peeled.as_commit().map(|c| c.time().seconds()).unwrap_or(0);
            tag_entries.push((time, name.to_string(), tag_oid, commit_oid));
        }
        tag_entries.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, name, tag_oid, commit_oid) in tag_entries {
            // The tag's peeled commit is intentionally NOT added to the
            // dedup set: showing both the tag and the underlying commit
            // lets the user explicitly opt out of recording a tag
            // association by picking the commit entry instead.
            entries.push(Entry::Tag {
                name,
                tag_oid,
                commit_oid,
            });
        }

        // Recent commits walked from HEAD. The default revwalk order
        // visits each commit before its parents, i.e. reverse
        // chronological on a linear history — good enough for a picker
        // cap at `PICKER_COMMIT_LIMIT`.
        if let Ok(mut revwalk) = raw.revwalk() {
            if revwalk.push_head().is_ok() {
                let mut added = 0usize;
                for oid_res in revwalk {
                    if added >= PICKER_COMMIT_LIMIT {
                        break;
                    }
                    let Ok(oid) = oid_res else { continue };
                    let commit_oid: Oid = oid.into();
                    entries.push(Entry::Commit { oid: commit_oid });
                    added += 1;
                }
            }
        }

        if entries.is_empty() {
            return Err("no commits or annotated tags found in this repository".into());
        }

        let labels: Vec<String> = entries
            .iter()
            .map(|entry| match entry {
                Entry::Tag {
                    name, commit_oid, ..
                } => {
                    let short = &commit_oid.to_string()[..7];
                    let title = display::CommitTitle::title(repo, commit_oid).unwrap_or_default();
                    format!("{name} -> {short}  {title}")
                }
                Entry::Commit { oid } => {
                    let short = &oid.to_string()[..7];
                    let title = display::CommitTitle::title(repo, oid).unwrap_or_default();
                    format!("{short}  {title}")
                }
            })
            .collect();

        let selection = inquire::Select::new("Select commit or tag:", labels)
            .raw_prompt()
            .map_err(|e| format!("selection cancelled: {e}"))?;
        Ok(entries[selection.index].resolved())
    }

    /// Cap on commits shown in the picker — enough to cover typical recent
    /// activity without overwhelming the terminal UI.
    const PICKER_COMMIT_LIMIT: usize = 30;

    enum Entry {
        Tag {
            name: String,
            tag_oid: Oid,
            commit_oid: Oid,
        },
        Commit {
            oid: Oid,
        },
    }

    impl Entry {
        fn resolved(&self) -> super::ResolvedRef {
            match self {
                Entry::Tag {
                    tag_oid,
                    commit_oid,
                    ..
                } => super::ResolvedRef {
                    commit: *commit_oid,
                    tag: Some(*tag_oid),
                },
                Entry::Commit { oid } => super::ResolvedRef {
                    commit: *oid,
                    tag: None,
                },
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

/// A git ref resolved to its commit OID, plus the annotated tag OID
/// when the ref was an annotated tag.
///
/// `commit` is always the commit the COB will be keyed by. `tag` is
/// `Some(tag_oid)` only when the user-provided ref pointed at an
/// annotated tag object; in that case the tag is recorded as release
/// metadata (see `Release::tag`). Lightweight tags (a ref pointing
/// directly at a commit) and bare commit hashes both produce
/// `tag = None`.
#[derive(Clone, Copy, Debug)]
struct ResolvedRef {
    commit: Oid,
    tag: Option<Oid>,
}

/// Resolve a git reference (full OID, short OID, or tag name) to a
/// commit OID, peeling annotated tags and reporting the tag's own OID
/// when applicable.
/// Resolve either `--release <id>` or a `<revision>` arg to a single
/// [`ReleaseId`]. Clap enforces that exactly one is provided.
///
/// The `--release` path verifies the id exists. The revision path
/// goes through [`Releases::find_unique_by_commit`] (delegate
/// priority); on ambiguity, the user is prompted to pick from the
/// candidates if interactive, or — when `no_input` or stdin isn't a
/// TTY — gets the existing `--release <id>` hint as an error.
fn resolve_target_release(
    release: Option<&str>,
    revision: Option<&str>,
    releases: &Releases<Repository>,
    repo: &Repository,
    delegates: &BTreeSet<Did>,
    no_input: bool,
    aliases: &impl AliasStore,
) -> Result<ReleaseId, error::ResolveTarget> {
    match (release, revision) {
        (Some(s), _) => {
            let id = parse_release_id(s, repo)?;
            match releases.get(&id) {
                Ok(Some(_)) => Ok(id),
                Ok(None) => Err(error::Find::NoReleaseId(id).into()),
                Err(err) => Err(error::Find::LookupId {
                    release_id: id,
                    err,
                }
                .into()),
            }
        }
        (None, Some(rev)) => {
            let oid = resolve_ref(rev, repo)?.commit;
            match releases.find_unique_by_commit(oid, delegates) {
                Ok(id) => Ok(id),
                Err(radicle_artifact::error::FindRelease::Ambiguous(_)) if !no_input => {
                    let candidates: Vec<_> = releases
                        .find_by_commit(oid)
                        .map_err(|err| error::Find::Lookup { oid, err })?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|err| error::Find::Lookup { oid, err })?;
                    prompt::pick_existing_release(&candidates, repo, aliases)
                        .map_err(error::ResolveTarget::Picker)
                }
                Err(e) => Err(error::Find::from(e).into()),
            }
        }
        (None, None) => unreachable!("clap requires one of --release or <revision>"),
    }
}

/// Resolve a (possibly abbreviated) release-id string to a full
/// [`ReleaseId`]. The COB's first commit is a real git object, so
/// `git revparse_single` accepts short OIDs, full OIDs, and any other
/// ref name that points at the COB.
fn parse_release_id(s: &str, repo: &Repository) -> Result<ReleaseId, error::Resolve> {
    let object = repo
        .raw()
        .revparse_single(s)
        .map_err(|err| error::Resolve {
            revision: s.to_owned(),
            err,
        })?;
    Ok(cob::ObjectId::from(object.id()).into())
}

fn resolve_ref(rev: &str, repo: &Repository) -> Result<ResolvedRef, error::Resolve> {
    use radicle::git::raw::ObjectType;

    let raw = repo.raw();
    let object = raw.revparse_single(rev).map_err(|err| error::Resolve {
        revision: rev.to_owned(),
        err,
    })?;
    if object.kind() == Some(ObjectType::Tag) {
        let tag_oid: Oid = object.id().into();
        let peeled = object
            .peel(ObjectType::Commit)
            .map_err(|err| error::Resolve {
                revision: rev.to_owned(),
                err,
            })?;
        Ok(ResolvedRef {
            commit: peeled.id().into(),
            tag: Some(tag_oid),
        })
    } else {
        Ok(ResolvedRef {
            commit: object.id().into(),
            tag: None,
        })
    }
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
    $ rad-artifact fetch v1.0 --cid baf...abc

  Fetch to a custom path:
    $ rad-artifact fetch v1.0 --cid baf...abc -o ./downloads/my-binary

  Fetch from a specific URL:
    $ rad-artifact fetch v1.0 --cid baf...abc --url https://example.com/my-binary")]
    pub struct Fetch {
        /// Git revision (commit, tag, or abbreviated OID). Required with --cid.
        #[clap(requires = "cid")]
        pub revision: Option<String>,
        /// Content identifier of the artifact to fetch. Required with <REVISION>.
        #[clap(long, requires = "revision")]
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
    /// The release revision and artifact name are prompted interactively
    /// when not given. Pass --revision and -n/--name to skip prompts (or
    /// use --no-input in scripts to fail instead of hanging on a prompt).
    /// Use --release to target an existing release directly by its id
    /// (skipping all commit/tag resolution).
    #[derive(Parser)]
    #[clap(
        group = clap::ArgGroup::new("source").required(true).args(["path", "cid"]),
        after_long_help = "\
Examples:
  Interactive: compute CID from a file, pick commit/tag, prompt for name:
    $ rad-artifact add ./my-binary

  Fully non-interactive:
    $ rad-artifact add ./my-binary --revision v1.0 --name \"my-binary v1.0\"

  Target an existing release by id (no commit/tag resolution):
    $ rad-artifact add ./my-binary --release <release-id> --name \"my-binary v1.0\"

  Register a precomputed CID without local bytes:
    $ rad-artifact add --cid baf...abc --revision v1.0 --name \"my-binary v1.0\""
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
        /// Git revision (commit, tag, or abbreviated OID) of the release.
        /// Prompts interactively when omitted. Conflicts with --release.
        #[clap(long, alias = "commit", conflicts_with = "release")]
        pub revision: Option<String>,
        /// Existing release id to add the artifact to. Skips commit/tag
        /// resolution and disambiguation; the release must already exist.
        #[clap(long)]
        pub release: Option<String>,
        /// Human-readable name for the artifact. Prompts interactively
        /// when omitted (with the path basename as the default).
        #[clap(short, long)]
        pub name: Option<String>,
    }

    /// Add a download location URL for an artifact CID
    ///
    /// Announces where an artifact can be retrieved from.
    ///
    /// Without --revision/--release and --cid, interactively lists
    /// releases and artifacts to pick from. The URL is always required.
    #[derive(Parser)]
    #[clap(
        group = clap::ArgGroup::new("target").args(["revision", "release"]),
        after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact location add https://example.com/my-binary

  Register an HTTPS download location:
    $ rad-artifact location add --revision v1.0 --cid baf...abc https://example.com/my-binary

  Register an iroh-blobs endpoint:
    $ rad-artifact location add --revision v1.0 --cid baf...abc iroh://<endpoint-id>

  Target a specific release by id:
    $ rad-artifact location add --release <release-id> --cid baf...abc https://example.com/my-binary"
    )]
    pub struct LocationAdd {
        /// Git revision (commit, tag, or abbreviated OID) of the release.
        /// Required with --cid unless --release is given.
        #[clap(long, requires = "cid")]
        pub revision: Option<String>,
        /// Existing release id. Skips commit/tag resolution. Required
        /// with --cid unless --revision is given.
        #[clap(long, requires = "cid")]
        pub release: Option<String>,
        /// Content identifier for the artifact. Required with a target
        /// (--revision or --release).
        #[clap(long, requires = "target")]
        pub cid: Option<Cid>,
        /// URL where the artifact can be retrieved.
        pub url: Url,
    }

    /// Attest that you verified an artifact CID
    ///
    /// Records that the signing node built from the same commit and
    /// obtained the same CID. Idempotent — attesting twice is a no-op.
    ///
    /// Without arguments, interactively lists releases and artifacts to
    /// pick from. Pass both <REVISION> and --cid to skip the prompts.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact attest

  Attest a specific artifact:
    $ rad-artifact attest v1.0 --cid baf...abc")]
    #[clap(group = clap::ArgGroup::new("target").args(["revision", "release"]))]
    pub struct Attest {
        /// Git revision (commit, tag, or abbreviated OID) of the release.
        /// Required with --cid unless --release is given.
        #[clap(requires = "cid")]
        pub revision: Option<String>,
        /// Existing release id. Skips commit/tag resolution. Required
        /// with --cid unless <revision> is given.
        #[clap(long, requires = "cid")]
        pub release: Option<String>,
        /// Content identifier for the artifact to attest. Required with
        /// a target (<revision> or --release).
        #[clap(long, requires = "target")]
        pub cid: Option<Cid>,
    }

    /// Redact an artifact CID, indicating it should not be used.
    ///
    /// Records that the signing node believes this artifact is compromised
    /// or should be withdrawn. The reason is a free-form string (max 2048
    /// bytes). The act of redaction is permanent; the reason text can be
    /// amended by redacting again. A redaction supersedes any prior
    /// attestation from the same DID.
    ///
    /// Without arguments, interactively lists releases and artifacts to
    /// pick from and prompts for a reason. Pass both <REVISION> and --cid
    /// to skip the release/artifact prompts; -m is still optional and
    /// will be prompted if omitted at a terminal.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact redact

  Redact a specific artifact:
    $ rad-artifact redact v1.0 --cid baf...abc -m \"build compromised, see advisory\"")]
    #[clap(group = clap::ArgGroup::new("target").args(["revision", "release"]))]
    pub struct Redact {
        /// Git revision (commit, tag, or abbreviated OID) of the release.
        /// Required with --cid unless --release is given.
        #[clap(requires = "cid")]
        pub revision: Option<String>,
        /// Existing release id. Skips commit/tag resolution. Required
        /// with --cid unless <revision> is given.
        #[clap(long, requires = "cid")]
        pub release: Option<String>,
        /// Content identifier for the artifact to redact. Required with
        /// a target (<revision> or --release).
        #[clap(long, requires = "target")]
        pub cid: Option<Cid>,
        /// Reason for the redaction.
        #[clap(short = 'm', long = "reason")]
        pub reason: Option<String>,
    }

    /// Remove a download location for an artifact.
    ///
    /// Retracts a previously announced location.
    ///
    /// Without arguments, interactively lists releases and artifacts to
    /// pick from, then prompts for the URL to remove from the locations
    /// you previously announced. Pass --revision/--release and --cid
    /// (and optionally <URL>) to skip the prompts.
    #[derive(Parser)]
    #[clap(
        group = clap::ArgGroup::new("target").args(["revision", "release"]),
        after_long_help = "\
Examples:
  Interactive mode (pick from your registered locations):
    $ rad-artifact location remove

  Remove a specific URL:
    $ rad-artifact location remove --revision v1.0 --cid baf...abc https://example.com/my-binary"
    )]
    pub struct LocationRemove {
        /// Git revision (commit, tag, or abbreviated OID) of the release.
        /// Required with --cid unless --release is given.
        #[clap(long, requires = "cid")]
        pub revision: Option<String>,
        /// Existing release id. Skips commit/tag resolution. Required
        /// with --cid unless --revision is given.
        #[clap(long, requires = "cid")]
        pub release: Option<String>,
        /// Content identifier for the artifact. Required with a target
        /// (--revision or --release).
        #[clap(long, requires = "target")]
        pub cid: Option<Cid>,
        /// URL to remove. Picked interactively from your registered
        /// locations when omitted.
        pub url: Option<Url>,
    }

    /// Show the release COB for a Git commit or annotated tag.
    ///
    /// By default only artifacts authored by a repository delegate are
    /// shown. Pass `--all-authors` to include artifacts added by other users.
    /// Pass `--release <id>` to target a specific release directly when
    /// multiple exist for the same commit.
    #[derive(Parser)]
    #[clap(
        group = clap::ArgGroup::new("target").required(true).args(["revision", "release"]),
        after_long_help = "\
Examples:
  Show a release as JSON (default):
    $ rad-artifact show v1.0

  Show a release in human-readable format:
    $ rad-artifact show --pretty v1.0

  Target a specific release by id (when multiple exist for the same commit):
    $ rad-artifact show --pretty --release <release-id>

  Include redacted artifacts and artifacts from non-delegates:
    $ rad-artifact show --pretty --redacted --all-authors v1.0"
    )]
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
        /// Git revision (commit, tag, or abbreviated OID) of the release.
        /// Conflicts with --release.
        pub revision: Option<String>,
        /// Existing release id. Skips commit/tag resolution and
        /// disambiguation. Conflicts with <revision>.
        #[clap(long)]
        pub release: Option<String>,
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
        #[error(transparent)]
        Delegates(#[from] Delegates),
        #[error("commit {oid} has {} existing release(s) that need disambiguation; pass --release <id> to pick one (candidates: {})", candidates.len(), display_ids(candidates))]
        NeedsDisambiguation {
            oid: Oid,
            candidates: Vec<ReleaseId>,
        },
        #[error("failed to create release for commit {oid}")]
        Create {
            oid: Oid,
            #[source]
            err: radicle_artifact::error::Create,
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

    fn display_ids(ids: &[ReleaseId]) -> String {
        ids.iter()
            .map(ReleaseId::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[derive(Debug, Error)]
    pub enum Locate {
        #[error("{0}")]
        Usage(String),
        #[error(transparent)]
        ResolveTarget(#[from] ResolveTarget),
        #[error(transparent)]
        Find(#[from] Find),
        #[error(transparent)]
        Delegates(#[from] Delegates),
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
        ResolveTarget(#[from] ResolveTarget),
        #[error(transparent)]
        Find(#[from] Find),
        #[error(transparent)]
        Delegates(#[from] Delegates),
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
        ResolveTarget(#[from] ResolveTarget),
        #[error(transparent)]
        Find(#[from] Find),
        #[error(transparent)]
        Delegates(#[from] Delegates),
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
        #[error("{0}")]
        Usage(String),
        #[error(transparent)]
        ResolveTarget(#[from] ResolveTarget),
        #[error(transparent)]
        Find(#[from] Find),
        #[error(transparent)]
        Delegates(#[from] Delegates),
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
        #[error("multiple non-delegate releases found for the commit {0} pass --release <id>")]
        Ambiguous(Oid),
        #[error("failed to find a release for the commit {oid}")]
        Lookup {
            oid: Oid,
            #[source]
            err: cob::store::Error,
        },
        #[error("no release found with id {0}")]
        NoReleaseId(ReleaseId),
        #[error("failed to look up release {release_id}")]
        LookupId {
            release_id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
    }

    /// Combined error for the `--release | <revision>` resolution path,
    /// shared by every subcommand that targets a single release.
    #[derive(Debug, Error)]
    pub enum ResolveTarget {
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("{0}")]
        Picker(String),
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
    #[error("could not resolve '{revision}' to a git object")]
    pub struct Resolve {
        pub revision: String,
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
