//! A cli to create and inspect artifact release COBs for a repository.
//!
//! Run `rad-artifact --help` to see how to use the cli.

use std::{collections::BTreeSet, error::Error as _, io::IsTerminal, time::Duration};

use clap::Parser;

use radicle::{
    cob, crypto,
    git::Oid,
    identity::Did,
    node::{AliasStore, Handle, Node},
    prelude::{Profile, ReadStorage, RepoId, WriteRepository},
    profile,
    storage::git::Repository,
};
use radicle_artifact::trust::{classify, Candidate, Untrusted};
use radicle_artifact::*;
use radicle_artifact_client::{sync::Client, DownloadArgs, FetchArgs};
use radicle_artifact_core::cid as share;
use radicle_artifact_core::keys::EndpointId;
use radicle_artifact_core::protocol::{Command, FetchLocation, FetchProgress, HasResult};
use url::Url;

mod node;
mod reconcile;
mod watch;

const TIMEOUT: Duration = Duration::from_millis(5000);

/// Per-frame idle bound for a streaming fetch. Larger than the node's own
/// download idle timeout so the node's "no progress" error reaches us
/// before this client-side cap fires.
pub(crate) const FETCH_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

fn main() {
    if let Err(err) = fallible_main() {
        let code = err.exit_code();
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
        std::process::exit(code);
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
    /// Operate on the given repository (default: cwd)
    ///
    /// The repository only has to be in local storage; no working copy is
    /// needed, so commands run from any directory.
    #[clap(short, long = "repo", alias = "repository", value_name = "RID")]
    #[clap(global = true)]
    repo: Option<RepoId>,

    /// Do not announce COB changes to the network after modifications.
    ///
    /// Use `rad sync -a` to announce at a later point.
    #[clap(long, global = true)]
    no_announce: bool,

    /// Disable all interactive prompts.
    ///
    /// Commands that would normally prompt (e.g. `fetch` without arguments)
    /// will error instead. Useful for scripts and CI.
    #[clap(long, global = true)]
    no_input: bool,

    #[clap(subcommand)]
    command: command::Command,
}

impl Args {
    fn repository(&self, profile: &Profile) -> Result<Repository, error::Repository> {
        open_repo(self.repo, profile)
    }
}

/// Open a repository: explicit `--repo <RID>` if given, otherwise
/// the radicle repo found by walking up from the cwd.
pub(crate) fn open_repo(
    repo_override: Option<RepoId>,
    profile: &Profile,
) -> Result<Repository, error::Repository> {
    let repo_id = if let Some(repo_id) = repo_override {
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

fn load_profile() -> Result<Profile, error::Profile> {
    Profile::load().map_err(error::Profile)
}

/// Open a repository's releases, sharing the node-wide artifact cache.
#[cfg(feature = "sqlite")]
pub(crate) fn open_releases<'a>(
    repo: &'a Repository,
    profile: &Profile,
) -> Result<Releases<'a, Repository>, error::Releases> {
    let db = cache_db_path(profile.cobs());
    Releases::open_cached(repo, db).map_err(|err| error::Releases { rid: repo.id, err })
}

/// Open a repository's releases without a cache; every read materializes from
/// git (correct, just slower).
#[cfg(not(feature = "sqlite"))]
pub(crate) fn open_releases<'a>(
    repo: &'a Repository,
    _profile: &Profile,
) -> Result<Releases<'a, Repository>, error::Releases> {
    log::debug!(target: "artifact", "built without the sqlite feature: artifact cache disabled");
    Releases::open(repo).map_err(|err| error::Releases { rid: repo.id, err })
}

/// Visibility rule for a release: by default show delegate-authored or
/// local-authored releases. `--all-authors` opens it up to everyone.
fn release_visible(
    release: &radicle_artifact::Release,
    delegates: &BTreeSet<Did>,
    local: &Did,
    all_authors: bool,
) -> bool {
    all_authors || delegates.contains(release.creator()) || release.creator() == local
}

pub(crate) fn announce(profile: &Profile, repo_id: RepoId) -> Result<(), error::Announce> {
    let mut node = Node::new(profile.home.socket_from_env());
    node.announce_refs_for(repo_id, [*profile.id()])
        .map_err(error::Announce)?;
    Ok(())
}

fn run(args: Args) -> Result<(), RadArtifactError> {
    use command::*;

    // The Cid subcommand doesn't need a profile or repo.
    if let Command::ComputeCid(cmd) = args.command {
        return run_cid(cmd);
    }

    // Subcommands that drive the daemon handle their own profile/repo
    // plumbing — some (`node start/stop/status/logs`) don't need a repo
    // at all; the rest open it through `open_repo`.
    if matches!(
        args.command,
        Command::Node(_)
            | Command::Seed(_)
            | Command::Unseed(_)
            | Command::Reconcile(_)
            | Command::Locate(_)
            | Command::Watch(_)
    ) {
        let profile = load_profile()?;
        let Args {
            command,
            repo,
            no_announce,
            no_input,
            ..
        } = args;
        return match command {
            Command::Node(cmd) => {
                node::run(cmd, repo, no_announce, no_input, &profile).map_err(Into::into)
            }
            Command::Seed(cmd) => run_seed(cmd, repo, no_announce, &profile).map_err(Into::into),
            Command::Unseed(cmd) => {
                run_unseed(cmd, repo, no_announce, no_input, &profile).map_err(Into::into)
            }
            Command::Reconcile(cmd) => reconcile::run(cmd, repo, &profile).map_err(Into::into),
            Command::Locate(cmd) => run_locate(cmd, &profile).map_err(Into::into),
            Command::Watch(cmd) => watch::run(cmd, no_announce, &profile).map_err(Into::into),
            _ => unreachable!(),
        };
    }

    let profile = load_profile()?;
    let repo = args.repository(&profile)?;
    let mut releases = open_releases(&repo, &profile)?;
    match args.command {
        Command::ComputeCid(_) => unreachable!(), // handled above
        Command::Node(_) => unreachable!(),       // handled above
        Command::Create(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            let (_, reused) =
                create_release(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
            // Reuse writes nothing to the COB, so there is nothing new to
            // gossip; only announce when a release was actually minted.
            if !reused && !args.no_announce {
                announce(&profile, repo.id)?;
            }
        }
        Command::Register(cmd) => {
            // Capture --seed before `cmd` is consumed; clap guarantees a
            // `<PATH>` is present whenever it is set (--seed conflicts with
            // --cid, and the source group is required).
            let seed = cmd.seed;
            let signer = profile.signer().map_err(error::Signer)?;
            let Registered {
                release_id,
                artifacts,
            } = register_artifact(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
            if seed {
                // Each artifact is seeded from its own file. A --each CID
                // carries the raw codec, so the node imports a single blob.
                for artifact in &artifacts {
                    let path = artifact
                        .path
                        .as_deref()
                        .expect("clap requires <PATH> with --seed");
                    node::seed_to_release(
                        path,
                        artifact.cid,
                        release_id,
                        false,
                        false,
                        repo.id,
                        &mut releases,
                        &profile,
                    )?;
                }
            }
            if !args.no_announce {
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
            if !args.no_announce {
                announce(&profile, repo.id)?;
            }
        }
        Command::Attest(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            attest_artifact(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
            if !args.no_announce {
                announce(&profile, repo.id)?;
            }
        }
        Command::Redact(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            redact_artifact(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
            if !args.no_announce {
                announce(&profile, repo.id)?;
            }
        }
        Command::Delete(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            delete_release(cmd, &mut releases, &repo, &signer)?;
            if !args.no_announce {
                announce(&profile, repo.id)?;
            }
        }
        Command::Metadata(meta) => {
            let signer = profile.signer().map_err(error::Signer)?;
            match meta.command {
                MetadataCommand::Set(cmd) => {
                    metadata_set(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
                }
                MetadataCommand::Unset(cmd) => {
                    metadata_unset(cmd, args.no_input, &mut releases, &repo, &profile, &signer)?;
                }
            }
            if !args.no_announce {
                announce(&profile, repo.id)?;
            }
        }
        Command::Show(cmd) => {
            let local = Did::from(*profile.id());
            show_release(
                cmd,
                &releases,
                &repo,
                releases.delegates(),
                &local,
                &profile,
            )?;
        }
        Command::List(cmd) => {
            let local = Did::from(*profile.id());
            list_releases(cmd, &releases, &repo, &local, &profile)?;
        }
        Command::Stats(cmd) => run_stats(cmd, &releases)?,
        Command::Verify(cmd) => {
            let local = Did::from(*profile.id());
            run_verify(cmd, &releases, &repo, &local, &profile)?;
        }
        Command::Fetch(cmd) => run_fetch(cmd, args.no_input, &profile, &releases, &repo)?,
        Command::Download(cmd) => run_download(cmd, args.no_input, &profile, &releases, &repo)?,
        // handled above
        Command::Locate(_)
        | Command::Seed(_)
        | Command::Unseed(_)
        | Command::Reconcile(_)
        | Command::Watch(_) => {
            unreachable!()
        }
    }

    Ok(())
}

/// Create (or reuse) a release for a commit and return its id.
///
/// Hoists the lazy release creation that `register` performs into an
/// explicit step, so a script can create once and pass `--release <id>`
/// to many `register` calls. Reuse is idempotent but deliberately
/// narrower than `register`'s resolution: it only ever reuses a release
/// the local user authored, and a tag fully determines intent — naming a
/// tag mints a fresh release when no own-release carries that exact tag,
/// without prompting. `register`, by contrast, resolves over all visible
/// releases (including delegates') and prompts on a tag conflict, because
/// its job is to find where to place an artifact rather than to open a
/// release for a specific commit+tag.
fn create_release<G>(
    command::Create { revision, json }: command::Create,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<(ReleaseId, bool), error::CreateRelease>
where
    G: crypto::Signer,
{
    let local = Did::from(*profile.id());

    // Resolve the revision to a commit (+ optional annotated tag).
    let resolved = match revision.as_deref() {
        Some(rev) => resolve_ref(rev, repo)?,
        None => prompt::pick_commit_or_tag(no_input, repo).map_err(error::CreateRelease::Usage)?,
    };
    let oid = resolved.commit;

    // Only releases we authored are reuse candidates.
    let mine: Vec<(ReleaseId, Release)> = collect_candidates(releases, oid)?
        .into_iter()
        .filter(|(_, r)| r.creator() == &local)
        .collect();

    // Decide whether to reuse an existing release or mint a fresh one.
    // A supplied tag fully determines intent (reuse iff a same-tag
    // release exists, else mint); a bare commit reuses a lone release and
    // disambiguates only when several exist.
    let reuse: Option<ReleaseId> = match resolved.tag {
        Some(tag) => mine
            .iter()
            .find(|(_, r)| r.tag() == Some(&tag))
            .map(|(id, _)| *id),
        None => match mine.as_slice() {
            [] => None,
            [(id, _)] => Some(*id),
            _ => {
                // Several of our own releases on this commit and nothing
                // to pick between them: prompt at a TTY, else fail with a
                // create-specific hint (there is no `--release` flag here).
                // No "create new" entry here — a bare commit asserts nothing
                // that would warrant minting yet another release.
                if no_input || !std::io::stdin().is_terminal() {
                    return Err(error::CreateRelease::Ambiguous {
                        oid,
                        candidates: mine.iter().map(|(id, _)| *id).collect(),
                    });
                }
                Some(
                    prompt::pick_existing_release(&mine, repo, profile)
                        .map_err(error::CreateRelease::Usage)?,
                )
            }
        },
    };

    let (id, reused) = match reuse {
        Some(id) => (id, true),
        None => {
            let release = releases
                .create(oid, resolved.tag, signer)
                .map_err(|err| error::CreateRelease::Create { oid, err })?;
            (*release.id(), false)
        }
    };

    // Machine-readable output: emit only the JSON object on stdout so the
    // release id is capturable without scraping stderr.
    if json {
        let out = display::CreateReceipt::new(id, oid);
        println!(
            "{}",
            serde_json::to_string(&out).map_err(error::CreateRelease::Json)?
        );
        return Ok((id, reused));
    }

    let short_oid = &oid.to_string()[..7];
    let short_id = &id.to_string()[..7];
    if reused {
        eprintln!("Reusing release {short_id} (commit {short_oid})");
    } else {
        eprintln!("Created release {short_id} (commit {short_oid})");
    }
    if std::io::stderr().is_terminal() {
        eprintln!(
            "Hint: register artifacts with `rad-artifact register <path> --release {short_id}`"
        );
    }
    // Bare id on stdout for scripting (`id=$(rad-artifact create v1.0)`).
    println!("{id}");
    Ok((id, reused))
}

/// One artifact waiting to go into the release COB.
struct Pending {
    cid: Cid,
    name: String,
    /// The `sizeBytes` hint to record, or `None` with --no-size or --cid.
    size: Option<u64>,
    /// Local file the bytes came from, or `None` when registering by --cid.
    path: Option<std::path::PathBuf>,
}

/// What `register` wrote: the release it landed in, and every artifact it put
/// there, so the caller can seed each one and announce once.
struct Registered {
    release_id: ReleaseId,
    artifacts: Vec<Pending>,
}

/// Where a registration gets its CID.
enum Source<'a> {
    Path(&'a std::path::Path),
    Cid(Cid),
}

/// Register one or more artifacts and return the release they landed in,
/// so the caller can reuse the CIDs for `--seed` and announce once.
fn register_artifact<G>(
    command::Register {
        path,
        cid,
        revision,
        release,
        name,
        seed,
        json,
        no_size,
        collection,
        each,
        all_authors,
    }: command::Register,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<Registered, error::Register>
where
    G: crypto::Signer,
{
    let interactive = !no_input && std::io::stdin().is_terminal();
    // ArgGroup guarantees exactly one of path/cid, but surface a usage
    // error rather than panic if clap ever changes its mind.
    let source = match (path.as_deref(), cid) {
        (Some(path), None) => Source::Path(path),
        (None, Some(cid)) => Source::Cid(cid),
        (Some(_), Some(_)) => {
            return Err(error::Register::Usage(
                "cannot pass --cid together with a path; the CID is computed from the contents"
                    .into(),
            ));
        }
        (None, None) => {
            return Err(error::Register::Usage(
                "missing artifact source; pass a <PATH> or --cid <CID>".into(),
            ));
        }
    };

    let plan = match source {
        Source::Path(p) => {
            registration_plan(p.is_dir(), collection, each, name.is_some(), interactive)
                .map_err(error::Register::Usage)?
        }
        // --cid has no local bytes to split.
        Source::Cid(_) => Plan::Blob,
    };

    // What --each passed over, reported once with the summary so nothing is
    // dropped silently.
    let mut skipped = Skipped::default();
    let pending: Vec<Pending> = match plan {
        Plan::Blob | Plan::Collection => vec![single_pending(&source, name, no_size, no_input)?],
        Plan::Ask | Plan::Each => {
            let Source::Path(dir) = source else {
                unreachable!("only a path can plan as a directory")
            };
            let (members, passed_over) = top_level_members(dir).map_err(error::Register::Io)?;
            let mode = match plan {
                Plan::Each => DirMode::Each,
                // Nothing to choose between when the top level holds no
                // files: keep the long-standing collection behaviour.
                _ if members.is_empty() => DirMode::Collection,
                _ => {
                    let tree = share::canonical_walk(dir).map_err(error::Register::Io)?;
                    let bytes = share::compute_size_from_path(dir).map_err(error::Register::Io)?;
                    let labels = dir_mode_labels(tree.len(), bytes, members.len());
                    prompt::pick_dir_mode(dir, labels).map_err(error::Register::Usage)?
                }
            };
            match mode {
                DirMode::Collection => vec![single_pending(&source, name, no_size, no_input)?],
                DirMode::Each => {
                    if members.is_empty() {
                        return Err(error::Register::Usage(format!(
                            "no files directly inside {}; register the whole tree with --collection",
                            dir.display()
                        )));
                    }
                    skipped = passed_over;
                    members
                        .into_iter()
                        .map(|m| {
                            Ok(Pending {
                                cid: share::compute_blob_cid(&m.path)
                                    .map_err(error::ComputeCid::Protocol)?,
                                name: m.name,
                                size: (!no_size).then_some(m.bytes),
                                path: Some(m.path),
                            })
                        })
                        .collect::<Result<_, error::Register>>()?
                }
            }
        }
    };

    let (release_id, oid) = resolve_register_target(
        release.as_deref(),
        revision.as_deref(),
        all_authors,
        no_input,
        releases,
        repo,
        profile,
        signer,
    )?;
    // One open release for every write: each `register_artifact*` call opens
    // its own signed transaction, so N artifacts are N COB entries.
    let mut release = releases
        .get_mut(&release_id)
        .map_err(|err| error::Register::Store {
            id: release_id,
            err,
        })?;
    for p in &pending {
        match p.size {
            Some(bytes) => {
                release.register_artifact_with_size(p.cid, p.name.clone(), bytes, signer)
            }
            None => release.register_artifact(p.cid, p.name.clone(), signer),
        }
        .map_err(|err| error::Register::Store {
            id: release_id,
            err,
        })?;
    }
    drop(release);

    // Machine-readable output: one JSON object per artifact, one per line, so
    // a single registration is byte-for-byte what it always was and a folder
    // stays readable with the same `jq` expression. Any --seed progress still
    // goes to stderr, keeping stdout clean.
    if json {
        for p in &pending {
            let out = display::RegisterReceipt::new(p.cid, p.name.clone(), release_id, oid, p.size);
            println!(
                "{}",
                serde_json::to_string(&out).map_err(error::Register::Json)?
            );
        }
        return Ok(Registered {
            release_id,
            artifacts: pending,
        });
    }

    let short_oid = &oid.to_string()[..7];
    let short_id = &release_id.to_string()[..7];
    if let [only] = pending.as_slice() {
        // Report the recorded size hint so the user knows the extra metadata
        // entry was written.
        let name = &only.name;
        match only.size {
            Some(bytes) => eprintln!(
                "Registered artifact '{name}' ({}) in release {short_id} (commit {short_oid})",
                display::human_bytes(bytes)
            ),
            None => {
                eprintln!("Registered artifact '{name}' in release {short_id} (commit {short_oid})")
            }
        }
    } else {
        // Every CID is on its own row: it is what a follow-up `attest`,
        // `redact`, or `location add` needs, and the single-artifact hint
        // below cannot carry more than one.
        for row in each_rows(&pending) {
            eprintln!("{row}");
        }
        let n = pending.len();
        let total: Option<u64> = pending.iter().map(|p| p.size).sum();
        match total {
            Some(bytes) => eprintln!(
                "Registered {n} artifacts ({}) in release {short_id} (commit {short_oid})",
                display::human_bytes(bytes)
            ),
            None => {
                eprintln!("Registered {n} artifacts in release {short_id} (commit {short_oid})")
            }
        }
    }
    // Outside the count: one registered file can still sit beside a dozen
    // subdirectories, and dropping those silently is the thing to avoid.
    for note in skipped.notes() {
        eprintln!("{note}");
    }
    // Skip the discovery hints when --seed is set: the caller is about to
    // seed and add a location, so they'd be noise.
    if !seed && std::io::stderr().is_terminal() {
        if let [only] = pending.as_slice() {
            eprintln!("Hint: use `rad-artifact location add --release {short_id} --cid {} <url>` to add a download location", only.cid);
            if let Some(p) = only.path.as_deref() {
                eprintln!(
                    "      or `rad-artifact seed {}` to seed it yourself via the local node",
                    p.display()
                );
            }
        } else {
            eprintln!(
                "Hint: re-run with --seed to serve these artifacts from your local node, or add a location per CID with `rad-artifact location add`"
            );
        }
    }
    // Bare CIDs on stdout for scripting (`cid=$(rad-artifact register …)`),
    // one per line. Suppressed in a TTY, where the summary already prints
    // them, to avoid redundant lone-CID lines.
    if !std::io::stdout().is_terminal() {
        for p in &pending {
            println!("{}", p.cid);
        }
    }
    Ok(Registered {
        release_id,
        artifacts: pending,
    })
}

/// The single-artifact case: one CID from `<PATH>` or --cid, with the name
/// prompted when it was not given.
fn single_pending(
    source: &Source,
    name: Option<String>,
    no_size: bool,
    no_input: bool,
) -> Result<Pending, error::Register> {
    let path = match source {
        Source::Path(p) => Some(*p),
        Source::Cid(_) => None,
    };
    let cid = match source {
        Source::Path(p) => compute_cid_from_path(p)?,
        Source::Cid(cid) => *cid,
    };
    // Record a size hint when registering from a local path; --cid alone has
    // no local bytes to measure, so it skips silently.
    let size = match (no_size, path) {
        (false, Some(p)) => Some(share::compute_size_from_path(p).map_err(error::Register::Io)?),
        _ => None,
    };
    let name = match name {
        Some(n) => n,
        None => {
            let default = path.and_then(|p| p.file_name()).and_then(|n| n.to_str());
            prompt::prompt_name(no_input, default).map_err(error::Register::Usage)?
        }
    };
    Ok(Pending {
        cid,
        name,
        size,
        path: path.map(std::path::Path::to_path_buf),
    })
}

/// One `name  cid  size` row per artifact, names padded so the CIDs line up.
/// The size is dropped when no hint was recorded (--no-size).
fn each_rows(artifacts: &[Pending]) -> Vec<String> {
    let width = artifacts
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(0);
    artifacts
        .iter()
        .map(|a| {
            let name = format!("{:width$}", a.name);
            match a.size {
                Some(bytes) => format!("  {name}  {}  {}", a.cid, display::human_bytes(bytes)),
                None => format!("  {name}  {}", a.cid),
            }
        })
        .collect()
}

/// Resolve --release / --revision / the commit prompt to the release that
/// artifacts land in, creating it when needed.
///
/// Returns ids rather than the open release, so the caller can open it once
/// and reuse that handle for every artifact it writes.
#[allow(clippy::too_many_arguments)]
fn resolve_register_target<G>(
    release: Option<&str>,
    revision: Option<&str>,
    all_authors: bool,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<(ReleaseId, Oid), error::Register>
where
    G: crypto::Signer,
{
    let delegates = releases.delegates().clone();
    let local = Did::from(*profile.id());
    let aliases = profile;

    match release {
        Some(s) => {
            let release_id = parse_release_id(s, repo)?;
            let release = releases.get_mut(&release_id).map_err(|err| match err {
                cob::store::Error::NotFound(_, _) => error::Find::NoReleaseId(release_id).into(),
                err => error::Register::Find(error::Find::LookupId { release_id, err }),
            })?;
            let oid = *release.oid();
            Ok((release_id, oid))
        }
        None => {
            let resolved = match revision {
                Some(rev) => resolve_ref(rev, repo)?,
                None => {
                    prompt::pick_commit_or_tag(no_input, repo).map_err(error::Register::Usage)?
                }
            };
            let oid = resolved.commit;
            let candidates: Vec<(ReleaseId, Release)> = collect_candidates(releases, oid)?
                .into_iter()
                .filter(|(_, r)| release_visible(r, &delegates, &local, all_authors))
                .collect();

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
                    .map_err(|err| error::Register::Create { oid, err })?
            } else if !needs_disambiguation {
                let id = candidates[0].0;
                releases
                    .get_mut(&id)
                    .map_err(|err| error::Register::Store { id, err })?
            } else if no_input {
                return Err(error::Register::NeedsDisambiguation {
                    oid,
                    candidates: candidates.iter().map(|(id, _)| *id).collect(),
                });
            } else {
                match prompt::pick_release_or_create(&candidates, resolved.tag, repo, aliases)
                    .map_err(error::Register::Usage)?
                {
                    prompt::ReleaseChoice::Existing(id) => releases
                        .get_mut(&id)
                        .map_err(|err| error::Register::Store { id, err })?,
                    prompt::ReleaseChoice::CreateNew => releases
                        .create(oid, resolved.tag, signer)
                        .map_err(|err| error::Register::Create { oid, err })?,
                }
            };
            Ok((*release.id(), oid))
        }
    }
}

/// Compute a CID by hashing the file or directory at `path`. Mirrors the
/// dispatch used by `serve` so both commands agree on what CID a given path
/// produces.
fn compute_cid_from_path(path: &std::path::Path) -> Result<Cid, error::ComputeCid> {
    if path.is_dir() {
        share::compute_content_id(path).map_err(error::ComputeCid::Io)
    } else {
        share::compute_blob_cid(path).map_err(error::ComputeCid::Protocol)
    }
}

/// What a `<PATH>` registers as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// A single file: one blob CID.
    Blob,
    /// A directory: one collection CID over the whole tree.
    Collection,
    /// A directory: one blob CID for each file directly inside it.
    Each,
    /// A directory, and the user has to choose.
    Ask,
}

/// The answer to [`Plan::Ask`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirMode {
    Collection,
    Each,
}

/// Decide what a local `<PATH>` registers as, without touching the terminal.
///
/// `interactive` is false with --no-input or when stdin is not a terminal; a
/// directory then stays one collection, which is what `register <DIR>` has
/// always done, so existing scripts keep their behaviour. A supplied
/// -n/--name also means one artifact, so it skips the question.
fn registration_plan(
    is_dir: bool,
    collection: bool,
    each: bool,
    named: bool,
    interactive: bool,
) -> Result<Plan, String> {
    match (is_dir, collection, each) {
        (false, false, false) => Ok(Plan::Blob),
        (false, _, _) => {
            Err("--collection and --each apply to a directory; <PATH> is a file".into())
        }
        (true, _, true) => Ok(Plan::Each),
        (true, true, _) => Ok(Plan::Collection),
        (true, false, false) if interactive && !named => Ok(Plan::Ask),
        (true, false, false) => Ok(Plan::Collection),
    }
}

/// A file directly inside a directory: the name it registers under, where to
/// read it, and its length.
struct Member {
    name: String,
    path: std::path::PathBuf,
    bytes: u64,
}

/// What `--each` passed over in a directory. Reported with the summary,
/// because an entry that is neither registered nor mentioned looks like an
/// entry that was never there.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Skipped {
    /// Subdirectories: --each does not recurse.
    dirs: usize,
    /// Entries --each cannot register: anything that is not a regular
    /// file, or whose name is not UTF-8.
    other: usize,
}

impl Skipped {
    /// One note per reason, in the order a reader meets them. Empty when
    /// the directory held nothing but registrable files.
    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.dirs > 0 {
            let n = self.dirs;
            let suffix = if n == 1 { "y" } else { "ies" };
            notes.push(format!(
                "Note: skipped {n} subdirector{suffix}; --each does not recurse"
            ));
        }
        if self.other > 0 {
            let n = self.other;
            let suffix = if n == 1 { "y" } else { "ies" };
            notes.push(format!(
                "Note: skipped {n} entr{suffix}: not a regular file, or the name is not UTF-8"
            ));
        }
        notes
    }
}

/// Files directly inside `dir`, sorted by name, plus what was passed over.
///
/// Unlike [`share::canonical_walk`], which recurses and is what the
/// collection CID covers, this reads one directory level. That is why the
/// names cannot collide: a single level cannot hold two entries with the
/// same name. Symlinks and anything that is not a regular file are skipped,
/// matching what the collection walk counts as a file.
fn top_level_members(dir: &std::path::Path) -> Result<(Vec<Member>, Skipped), std::io::Error> {
    let mut members = Vec::new();
    let mut skipped = Skipped::default();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // `file_type` does not follow symlinks, so a link is never mistaken
        // for the file or directory it points at.
        let kind = entry.file_type()?;
        if kind.is_dir() {
            skipped.dirs += 1;
            continue;
        }
        if !kind.is_file() {
            skipped.other += 1;
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            skipped.other += 1;
            continue;
        };
        members.push(Member {
            name,
            path: entry.path(),
            bytes: entry.metadata()?.len(),
        });
    }

    members.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((members, skipped))
}

/// The two options the directory question offers, in order. The counts differ
/// because a collection covers the whole tree and --each covers one level.
fn dir_mode_labels(tree_files: usize, tree_bytes: u64, top_level: usize) -> [String; 2] {
    [
        format!(
            "one collection artifact ({tree_files} file{}, {})",
            plural(tree_files),
            display::human_bytes(tree_bytes)
        ),
        format!(
            "{top_level} separate artifact{} (top-level files only)",
            plural(top_level)
        ),
    ]
}

/// Plural suffix for a count, so the prompts and summaries read correctly.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn location_add<G>(
    command::LocationAdd {
        revision,
        release,
        cid,
        url,
        all_authors,
    }: command::LocationAdd,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<(), error::Locate>
where
    G: crypto::Signer,
{
    let delegates = releases.delegates().clone();
    let local = Did::from(*profile.id());
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
                &local,
                all_authors,
                no_input,
                profile,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) =
                prompt::pick_interactive(no_input, releases, repo).map_err(error::Locate::Usage)?;
            let id = resolve_release_after_pick(
                oid,
                &cid,
                releases,
                repo,
                profile,
                &delegates,
                &local,
                all_authors,
            )?;
            (id, cid)
        }
        _ => unreachable!("clap enforces a target arg with --cid and vice versa"),
    };
    // Reject the renamed legacy scheme up front rather than silently
    // signing an unfetchable `iroh://` URL into the COB.
    if EndpointId::is_legacy_endpoint_url(&url) {
        return Err(error::Locate::Usage(
            "the `iroh://` scheme was renamed to `radiroh://`; \
             re-add this location with the new scheme"
                .into(),
        ));
    }
    // Validate radiroh:// URLs up front so a typo in the endpoint id surfaces
    // here, before it ends up signed into the COB. A bare radiroh:// is allowed
    // and resolves to the author's DID-derived endpoint id at fetch time.
    if EndpointId::is_endpoint_url(&url) {
        EndpointId::from_url(&url).map_err(|e| error::Locate::Usage(e.to_string()))?;
    }
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Locate::Store { id, err })?;
    release
        .add_location(cid, url.clone(), signer)
        .map_err(|err| error::Locate::Store { id, err })?;
    eprintln!("Added location {url} for artifact {cid}");
    Ok(())
}

fn attest_artifact<G>(
    command::Attest {
        revision,
        release,
        cid,
        all_authors,
    }: command::Attest,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<(), error::Attest>
where
    G: crypto::Signer,
{
    let delegates = releases.delegates().clone();
    let local = Did::from(*profile.id());
    let release = release.as_deref();
    let revision = revision.as_deref();
    let (id, cid) = match (release, revision, cid) {
        (Some(_), _, Some(cid)) | (_, Some(_), Some(cid)) => {
            let id = resolve_target_release(
                release,
                revision,
                releases,
                repo,
                &delegates,
                &local,
                all_authors,
                no_input,
                profile,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) =
                prompt::pick_interactive(no_input, releases, repo).map_err(error::Attest::Usage)?;
            let id = resolve_release_after_pick(
                oid,
                &cid,
                releases,
                repo,
                profile,
                &delegates,
                &local,
                all_authors,
            )?;
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
        all_authors,
    }: command::Redact,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<(), error::Redact>
where
    G: crypto::Signer,
{
    let delegates = releases.delegates().clone();
    let local = Did::from(*profile.id());
    let release = release.as_deref();
    let revision = revision.as_deref();
    let (id, cid) = match (release, revision, cid) {
        (Some(_), _, Some(cid)) | (_, Some(_), Some(cid)) => {
            let id = resolve_target_release(
                release,
                revision,
                releases,
                repo,
                &delegates,
                &local,
                all_authors,
                no_input,
                profile,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) =
                prompt::pick_interactive(no_input, releases, repo).map_err(error::Redact::Usage)?;
            let id = resolve_release_after_pick(
                oid,
                &cid,
                releases,
                repo,
                profile,
                &delegates,
                &local,
                all_authors,
            )?;
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

fn delete_release<G>(
    command::Delete { release }: command::Delete,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    signer: &G,
) -> Result<(), error::Delete>
where
    G: crypto::Signer,
{
    let id = parse_release_id(&release, repo)?;
    match releases.get(&id) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(error::Find::NoReleaseId(id).into()),
        Err(err) => {
            return Err(error::Find::LookupId {
                release_id: id,
                err,
            }
            .into())
        }
    }
    releases
        .remove(&id, signer)
        .map_err(|err| error::Delete::Store { id, err })?;
    eprintln!("Deleted release {id}");
    Ok(())
}

/// Resolve the (release, cid) target and check that the local DID is
/// authorized to mutate metadata on the targeted artifact — i.e. is the
/// artifact's author or a current repo delegate.
#[allow(clippy::too_many_arguments)]
fn resolve_metadata_target(
    release: Option<&str>,
    revision: Option<&str>,
    cid: Option<Cid>,
    all_authors: bool,
    no_input: bool,
    releases: &Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    delegates: &BTreeSet<Did>,
) -> Result<(ReleaseId, Cid), error::Metadata> {
    let local = Did::from(*profile.id());
    let (id, cid) = match (release, revision, cid) {
        (Some(_), _, Some(cid)) | (_, Some(_), Some(cid)) => {
            let id = resolve_target_release(
                release,
                revision,
                releases,
                repo,
                delegates,
                &local,
                all_authors,
                no_input,
                profile,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) = prompt::pick_interactive(no_input, releases, repo)
                .map_err(error::Metadata::Usage)?;
            let id = resolve_release_after_pick(
                oid,
                &cid,
                releases,
                repo,
                profile,
                delegates,
                &local,
                all_authors,
            )?;
            (id, cid)
        }
        _ => unreachable!("clap enforces a target arg with --cid and vice versa"),
    };

    // Look up the artifact to find its author. The lookup also doubles
    // as a "release/cid exists" precondition with a clear error.
    let r = releases
        .get(&id)
        .map_err(|err| error::Metadata::Store { id, err })?
        .ok_or(error::Metadata::Find(error::Find::NoReleaseId(id)))?;
    let artifact_author = *r
        .artifact(&cid)
        .ok_or(error::Metadata::UnknownCid { id, cid })?
        .author();

    let authorized = local == artifact_author || delegates.contains(&local);
    if !authorized {
        return Err(error::Metadata::NotAuthorized {
            local: Box::new(local),
            artifact_author: Box::new(artifact_author),
            cid: Box::new(cid),
        });
    }
    Ok((id, cid))
}

fn metadata_set<G>(
    command::MetadataSet {
        revision,
        release,
        cid,
        json,
        key,
        value,
        all_authors,
    }: command::MetadataSet,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<(), error::Metadata>
where
    G: crypto::Signer,
{
    let delegates = releases.delegates().clone();
    let (id, cid) = resolve_metadata_target(
        release.as_deref(),
        revision.as_deref(),
        cid,
        all_authors,
        no_input,
        releases,
        repo,
        profile,
        &delegates,
    )?;
    // With --json, parse the value as JSON; otherwise store it verbatim
    // as a JSON string so simple `key=value` invocations don't need quoting.
    let value = if json {
        serde_json::from_str(&value).map_err(|err| error::Metadata::InvalidJson { err })?
    } else {
        serde_json::Value::String(value)
    };
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Metadata::Store { id, err })?;
    release
        .set_metadata(cid, key.clone(), value, signer)
        .map_err(|err| match err {
            radicle_artifact::error::Metadata::Store(err) => error::Metadata::Store { id, err },
            err => error::Metadata::InvalidKey(err),
        })?;
    eprintln!("Set metadata {key} on artifact {cid}");
    Ok(())
}

fn metadata_unset<G>(
    command::MetadataUnset {
        revision,
        release,
        cid,
        key,
        all_authors,
    }: command::MetadataUnset,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<(), error::Metadata>
where
    G: crypto::Signer,
{
    let delegates = releases.delegates().clone();
    let (id, cid) = resolve_metadata_target(
        release.as_deref(),
        revision.as_deref(),
        cid,
        all_authors,
        no_input,
        releases,
        repo,
        profile,
        &delegates,
    )?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Metadata::Store { id, err })?;
    release
        .remove_metadata(cid, key.clone(), signer)
        .map_err(|err| error::Metadata::Store { id, err })?;
    eprintln!("Removed metadata {key} from artifact {cid}");
    Ok(())
}

fn location_remove<G>(
    command::LocationRemove {
        revision,
        release,
        cid,
        url,
        all_authors,
    }: command::LocationRemove,
    no_input: bool,
    releases: &mut Releases<Repository>,
    repo: &Repository,
    profile: &Profile,
    signer: &G,
) -> Result<(), error::RemoveLocation>
where
    G: crypto::Signer,
{
    let delegates = releases.delegates().clone();
    let local = Did::from(*profile.id());
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
                &local,
                all_authors,
                no_input,
                profile,
            )?;
            (id, cid)
        }
        (None, None, None) => {
            let (oid, cid) = prompt::pick_interactive(no_input, releases, repo)
                .map_err(error::RemoveLocation::Usage)?;
            let id = resolve_release_after_pick(
                oid,
                &cid,
                releases,
                repo,
                profile,
                &delegates,
                &local,
                all_authors,
            )?;
            (id, cid)
        }
        _ => unreachable!("clap enforces a target arg with --cid and vice versa"),
    };

    // Look up the user's own added locations for this artifact so we
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
                    "you have not added location {url} for artifact {cid}"
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
                let hits: Vec<_> = collect_candidates(releases, oid)?
                    .into_iter()
                    .filter(|(_, r)| release_visible(r, delegates, local, all_authors))
                    .collect();
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
    // Delegates drive both the redaction and author filters; the flags
    // below bypass each filter independently.
    let delegates = releases.delegates();
    let iter = releases
        .all()
        .map_err(error::List::All)?
        .into_iter()
        .filter_map(|res| match res {
            Ok((id, release)) => Some((ReleaseId::from(id), release)),
            Err(err) => {
                if verbose {
                    eprintln!("Failed to retrieve release: {err}");
                }
                None
            }
        })
        .filter(|(_, release)| release_visible(release, delegates, local, all_authors));
    let filters = display::Filters {
        delegates,
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

/// Report repository-wide figures.
///
/// The delegate set is resolved here and handed to `counts`, so the
/// figures use the same trust view as `list` in the same working copy.
fn run_stats(
    command::Stats { pretty, json }: command::Stats,
    releases: &Releases<Repository>,
) -> Result<(), error::Stats> {
    let counts = releases.counts().map_err(error::Stats::Counts)?;
    let stats = display::Stats::new(&counts);
    if use_pretty(pretty, json) {
        print!("{}", stats.pretty(pretty_style(false)));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&stats).map_err(error::Stats::Json)?
        );
    }
    Ok(())
}

/// Pick which failure to report when no candidate verified.
///
/// A redaction is a deliberate act by a trusted party, so it outranks "a
/// stranger registered it", which in turn outranks "nothing registered
/// these bytes".
fn rejection(untrusted: &[Untrusted], cid: &Cid, rid: RepoId) -> error::Verify {
    for reason in untrusted {
        if let Untrusted::Redacted(redactions) = reason {
            return error::Verify::Redacted {
                cid: cid.to_string(),
                redactions: redactions.clone(),
            };
        }
    }
    if let Some(Untrusted::Author(author)) = untrusted.first() {
        return error::Verify::UntrustedAuthor {
            cid: cid.to_string(),
            author: *author,
        };
    }
    error::Verify::NoMatch {
        cid: cid.to_string(),
        rid,
    }
}

/// The `--json` payload for a rejection, or `None` when the check could
/// not be made at all and there is no verdict to report.
fn verify_failure(cid: Cid, err: &error::Verify) -> Option<display::VerifyFailure> {
    use std::collections::BTreeMap;

    let (reason, redactions) = match err {
        error::Verify::NoMatch { .. } => ("noMatch", BTreeMap::new()),
        error::Verify::Redacted { redactions, .. } => ("redacted", redactions.clone()),
        error::Verify::UntrustedAuthor { .. } => ("untrustedAuthor", BTreeMap::new()),
        _ => return None,
    };
    Some(display::VerifyFailure::new(
        cid,
        reason,
        err.to_string(),
        redactions,
    ))
}

/// Verify that a local file matches an artifact registered in this
/// repository, and print what registered it.
fn run_verify(
    command::Verify {
        path,
        all_authors,
        pretty,
        json,
    }: command::Verify,
    releases: &Releases<Repository>,
    repo: &Repository,
    local: &Did,
    aliases: &impl AliasStore,
) -> Result<(), error::Verify> {
    let delegates = releases.delegates();
    let cid = compute_cid_from_path(&path)?;

    // Keyed by the CID we just computed, so the file itself picks the
    // candidates. A CID legitimately appears in more than one release (the
    // same build reused across commits, or duplicate release COBs for one
    // commit), so we report every match rather than choosing between them.
    let candidates = releases
        .find_by_cid(&cid)
        .map_err(|err| error::Verify::Lookup {
            cid: cid.to_string(),
            err: Box::new(err),
        })?;

    let mut matched = Vec::new();
    let mut untrusted = Vec::new();
    for (release_id, release) in candidates {
        let artifact = release
            .artifact(&cid)
            .expect("find_by_cid only returns releases containing the CID");
        match classify(
            &Candidate::new(&release, artifact),
            delegates,
            local,
            all_authors,
        ) {
            Ok(()) => matched.push(display::VerifyMatch::new(
                release_id,
                &release,
                artifact,
                aliases,
                repo,
                delegates.contains(artifact.author()),
            )),
            Err(reason) => untrusted.push(reason),
        }
    }

    if matched.is_empty() {
        let err = rejection(&untrusted, &cid, repo.id);
        // A caller that asked for JSON gets the verdict as data, not just
        // an exit code. Only a verdict is emitted: a failure that could not
        // answer the question prints the error alone.
        if !use_pretty(pretty, json) {
            if let Some(failure) = verify_failure(cid, &err) {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&failure).map_err(error::Verify::Json)?
                );
            }
        }
        return Err(err);
    }

    let receipt = display::VerifyReceipt::new(cid, matched);
    if use_pretty(pretty, json) {
        print!("{}", receipt.pretty(pretty_style(false)));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&receipt).map_err(error::Verify::Json)?
        );
    }
    Ok(())
}

/// Locate a CID across every repository in local storage, printing JSON.
///
/// This is a node-wide query, so it uses `profile.storage` directly rather than
/// a single repository and ignores `--repo`.
fn run_locate(cmd: command::Locate, profile: &Profile) -> Result<(), error::Locations> {
    let command::Locate { cid, releases } = cmd;
    #[cfg(feature = "sqlite")]
    let index = radicle_artifact::discovery::Index::open_cached(
        &profile.storage,
        cache_db_path(profile.cobs()),
    );
    #[cfg(not(feature = "sqlite"))]
    let index = {
        log::debug!(target: "artifact", "built without the sqlite feature: artifact cache disabled");
        radicle_artifact::discovery::Index::open(&profile.storage)
    };
    let json = if releases {
        let matches = index
            .releases_by_cid(&cid)
            .map_err(error::Locations::Storage)?;
        serde_json::to_string_pretty(&matches).map_err(error::Locations::Json)?
    } else {
        let locations = index
            .locations_by_cid(&cid)
            .map_err(error::Locations::Storage)?;
        serde_json::to_string_pretty(&locations).map_err(error::Locations::Json)?
    };
    println!("{json}");
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
        let hash = blake3::hash(&data);
        let cid = share::blake3_hash_to_cid(hash, share::ArtifactKind::Blob);
        println!("{cid}");
    }
    Ok(())
}

/// What `fetch`/`download` need from the COB once the target is resolved:
/// the CID, the union of locations to try, and the release to add a
/// `--seed` location into.
struct Retrieval {
    cid: Cid,
    locations: Vec<FetchLocation>,
    /// Release matching the requested revision (else any containing the
    /// CID) — the one a `--seed` location is added to.
    primary_id: ReleaseId,
    /// Artifact name, for the default download output path.
    name: String,
}

/// Resolve the `(revision, cid)` pair (interactively when both are absent),
/// union locations across every release containing the CID, and warn on
/// redactions. Shared by [`run_fetch`] and [`run_download`].
fn resolve_retrieval(
    revision: Option<String>,
    cid_arg: Option<Cid>,
    url: Option<&Url>,
    no_input: bool,
    require_locations: bool,
    releases: &Releases<Repository>,
    repo: &Repository,
) -> Result<Retrieval, RadArtifactError> {
    let (oid_opt, cid) = match (revision, cid_arg) {
        (Some(revision), Some(cid)) => {
            let oid = resolve_ref(&revision, repo)?.commit;
            (Some(oid), cid)
        }
        (None, Some(cid)) => (None, cid),
        (None, None) => {
            let (oid, cid) =
                prompt::pick_interactive(no_input, releases, repo).map_err(error::Share::Usage)?;
            (Some(oid), cid)
        }
        (Some(_), None) => unreachable!("revision requires cid"),
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
    // Sanity check: when a revision was given, at least one of its releases
    // must contain this CID. Catches callers who pair a valid CID with the
    // wrong OID.
    if let Some(oid) = oid_opt {
        if !matching.iter().any(|(_, r)| r.oid() == &oid) {
            return Err(error::Share::Usage(format!(
                "artifact {cid} is not associated with commit {oid}"
            ))
            .into());
        }
    }

    // When an OID is known, prefer its release for name/redactions. For
    // CID-only lookups, pick the most recently created release (mirrors the
    // `seed` subcommand's policy when multiple releases share a CID).
    let primary = match oid_opt {
        Some(oid) => matching
            .iter()
            .find(|(_, r)| r.oid() == &oid)
            .or_else(|| matching.first()),
        None => matching.iter().max_by_key(|(_, r)| r.timestamp()),
    }
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

    let primary_id = primary.0;
    let name = artifact.name().to_owned();

    let locations = if let Some(url) = url {
        vec![FetchLocation::Url(url.clone())]
    } else {
        let artifacts = matching.iter().filter_map(|(_, r)| r.artifact(&cid));
        artifact_locations(artifacts)?
    };
    // Short-circuit when no usable source exists. Skipped for offline export,
    // where the bytes are read from the store and no location is needed.
    if locations.is_empty() && require_locations {
        return Err(error::Share::NoLocationsForCid { cid }.into());
    }

    Ok(Retrieval {
        cid,
        locations,
        primary_id,
        name,
    })
}

/// Build the spinner shared by `fetch`/`download` progress reporting.
fn retrieval_progress_bar() -> indicatif::ProgressBar {
    let pb = indicatif::ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(250));
    pb.set_style(
        indicatif::ProgressStyle::with_template(
            "{spinner:.green} {msg} {bytes} ({binary_bytes_per_sec})",
        )
        .unwrap(),
    );
    pb
}

/// Render a progress frame onto the spinner, shared by `fetch`/`download`.
/// The frame-to-update mapping lives in [`display::describe_progress`] so a
/// new `FetchProgress` variant is a compile error there, not a silent drop.
fn apply_progress(p: &FetchProgress, pb: &indicatif::ProgressBar) {
    match display::describe_progress(p) {
        Some(display::ProgressUpdate::Message(m)) => pb.set_message(m),
        Some(display::ProgressUpdate::Position(offset)) => pb.set_position(offset),
        None => {}
    }
}

/// On `--seed`, the node now serves the bytes; add a discoverable
/// location with a signed COB write (the node writes no COBs itself).
pub(crate) fn add_seed_location(
    endpoint_id: EndpointId,
    cid: Cid,
    primary_id: ReleaseId,
    repo: &Repository,
    profile: &Profile,
) -> Result<(), RadArtifactError> {
    let mut releases_mut = open_releases(repo, profile)?;
    let signer = profile
        .signer()
        .map_err(|e| error::Share::Usage(format!("signer: {e}")))?;
    let url = endpoint_id.to_url();
    let mut release_mut = releases_mut
        .get_mut(&primary_id)
        .map_err(|e| error::Share::Usage(format!("open release {primary_id}: {e}")))?;
    release_mut
        .add_location(cid, url, &signer)
        .map_err(|e| error::Share::Usage(format!("add location: {e}")))?;
    let short_id = &primary_id.to_string()[..7];
    eprintln!("Now seeding {cid}; added location to release {short_id}");
    Ok(())
}

/// Log whether the artifact is already cached or list the locations to try.
/// Returns `true` if the bytes are already complete in the local store.
fn log_retrieval_plan(client: &Client, cid: Cid, locations: &[FetchLocation]) -> bool {
    let already_local = client
        .call::<HasResult>(&Command::Has { cid }, TIMEOUT)
        .map(|h| h.complete)
        .unwrap_or(false);

    if already_local {
        eprintln!("Already in local store");
    } else {
        let (url_count, iroh_count) =
            locations
                .iter()
                .fold((0usize, 0usize), |(u, i), loc| match loc {
                    FetchLocation::Url(_) => (u + 1, i),
                    FetchLocation::Iroh(_) => (u, i + 1),
                });
        eprintln!(
            "Trying {} location{} ({url_count} url, {iroh_count} iroh)...",
            locations.len(),
            if locations.len() == 1 { "" } else { "s" },
        );
    }
    already_local
}

/// `fetch`: pull an artifact into the local node's store without writing it
/// to disk. Use `download` to also export the bytes to a file.
fn run_fetch(
    args: command::Fetch,
    no_input: bool,
    profile: &Profile,
    releases: &Releases<Repository>,
    repo: &Repository,
) -> Result<(), RadArtifactError> {
    let Retrieval {
        cid,
        locations,
        primary_id,
        ..
    } = resolve_retrieval(
        args.revision,
        args.cid,
        args.url.as_ref(),
        no_input,
        true,
        releases,
        repo,
    )?;

    // Route through the local node, which owns the store and all blob I/O.
    // A missing node surfaces as `node::Error::NotRunning`.
    let client = Client::new(Client::default_socket(profile.home.path()));

    let already_local = log_retrieval_plan(&client, cid, &locations);

    // Bytes are already present and there's no seeded tag to set — nothing to do.
    if already_local && !args.seed {
        return Ok(());
    }

    let fetch_args = FetchArgs {
        rid: repo.id,
        cid,
        locations,
        // Seed under the same release the location is announced to below, so
        // the tag and the COB annotation stay in sync.
        seed: args.seed.then_some(primary_id.oid()),
    };

    let pb = retrieval_progress_bar();
    let receipt = client
        .fetch(fetch_args, FETCH_IDLE_TIMEOUT, |p| apply_progress(p, &pb))
        .map_err(node::client_err)?;
    pb.finish_and_clear();

    if args.seed && receipt.seeded {
        add_seed_location(receipt.endpoint_id, cid, primary_id, repo, profile)?;
    }

    // The cache path already reported "Already in local store"; only the
    // seeded tag was new, so don't claim a fetch happened.
    if !receipt.from_cache {
        eprintln!("Fetched {cid} into the store");
    }
    Ok(())
}

/// `download`: fetch an artifact into the store and export it to disk.
fn run_download(
    args: command::Download,
    no_input: bool,
    profile: &Profile,
    releases: &Releases<Repository>,
    repo: &Repository,
) -> Result<(), RadArtifactError> {
    let Retrieval {
        cid,
        locations,
        primary_id,
        name,
    } = resolve_retrieval(
        args.revision,
        args.cid,
        args.url.as_ref(),
        no_input,
        !args.offline,
        releases,
        repo,
    )?;

    let requested_path = args
        .output
        .unwrap_or_else(|| std::path::PathBuf::from(name.replace(' ', "_")));

    // Resolve the download destination to an absolute path.
    //
    // The node runs as a daemon with a different cwd, so a relative path would
    // resolve to the wrong location once handed to it.
    let output_path = std::path::absolute(&requested_path).unwrap_or(requested_path);

    // Route through the local node, which owns the store and all blob I/O.
    // A missing node surfaces as `node::Error::NotRunning`.
    let client = Client::new(Client::default_socket(profile.home.path()));

    // `--offline`: export straight from the store, never touching the network.
    // Errors with `NotLocal` if the bytes aren't already complete locally.
    if args.offline {
        let pb = retrieval_progress_bar();
        client
            .export(cid, output_path.clone(), FETCH_IDLE_TIMEOUT, |p| {
                apply_progress(p, &pb)
            })
            .map_err(node::client_err)?;
        pb.finish_and_clear();
        eprintln!("Saved to {}", output_path.display());
        return Ok(());
    }

    log_retrieval_plan(&client, cid, &locations);

    let download_args = DownloadArgs {
        rid: repo.id,
        cid,
        locations,
        dest: output_path.clone(),
        // Seed under the same release the location is announced to below, so
        // the tag and the COB annotation stay in sync.
        seed: args.seed.then_some(primary_id.oid()),
    };

    let pb = retrieval_progress_bar();
    let receipt = client
        .download(download_args, FETCH_IDLE_TIMEOUT, |p| {
            apply_progress(p, &pb)
        })
        .map_err(node::client_err)?;
    pb.finish_and_clear();

    if args.seed && receipt.seeded {
        add_seed_location(receipt.endpoint_id, cid, primary_id, repo, profile)?;
    }

    eprintln!("Saved to {}", output_path.display());
    Ok(())
}

/// Top-level alias for `rad-artifact node seed <PATH>`.
fn run_seed(
    cmd: command::Seed,
    repo_override: Option<RepoId>,
    no_announce: bool,
    profile: &Profile,
) -> Result<(), node::Error> {
    node::seed_artifact(
        cmd.path,
        cmd.release,
        cmd.reference,
        cmd.no_location,
        no_announce,
        repo_override,
        profile,
    )
}

/// Top-level `rad-artifact unseed --cid <CID>`.
fn run_unseed(
    cmd: command::Unseed,
    repo_override: Option<RepoId>,
    no_announce: bool,
    no_input: bool,
    profile: &Profile,
) -> Result<(), node::Error> {
    node::unseed_artifact(
        cmd.cid,
        cmd.release,
        no_announce,
        no_input,
        repo_override,
        profile,
    )
}

/// Convert locations from one or more artifacts into fetch locations.
///
/// For `radiroh://<endpoint-id>` URLs, parses the endpoint id from the URL host.
/// For bare `radiroh://` URLs, derives the endpoint id from the DID that authored
/// the location (same Ed25519 key). Locations are deduplicated across
/// artifacts — plain URLs collapse on URL equality, and iroh entries collapse
/// on resolved endpoint id regardless of how many releases or DIDs contributed
/// them. URLs whose iroh host fails to parse are skipped with a warning on
/// stderr so that one bad entry doesn't sink an otherwise-fetchable artifact.
pub(crate) fn artifact_locations<'a>(
    artifacts: impl IntoIterator<Item = &'a Artifact>,
) -> Result<Vec<FetchLocation>, RadArtifactError> {
    let mut seen_urls: BTreeSet<&url::Url> = BTreeSet::new();
    let mut seen_iroh: BTreeSet<EndpointId> = BTreeSet::new();
    let mut locations = Vec::new();
    for artifact in artifacts {
        for (did, urls) in artifact.locations() {
            for url in urls {
                if EndpointId::is_endpoint_url(url) {
                    let endpoint_id = match EndpointId::from_url(url) {
                        Ok(Some(id)) => id,
                        Ok(None) => EndpointId::try_from(did).map_err(error::Share::Protocol)?,
                        Err(e) => {
                            eprintln!("Warning: skipping location {url}: {e}");
                            continue;
                        }
                    };
                    if seen_iroh.insert(endpoint_id) {
                        locations.push(FetchLocation::Iroh(endpoint_id));
                    }
                } else if seen_urls.insert(url) {
                    locations.push(FetchLocation::Url(url.clone()));
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

    /// Whether an unseed drops a single release's tag or sweeps the CID
    /// across every release that references it.
    pub enum UnseedScope {
        One(ReleaseId),
        All,
    }

    /// Disambiguate which release(s) to unseed when a CID belongs to more
    /// than one and no `--release` was given. Offers each candidate plus an
    /// explicit "All releases" entry so the sweep-everything behavior stays
    /// reachable. Requires a TTY; callers gate on `no_input` first.
    pub fn pick_unseed_release(
        candidates: &[(ReleaseId, Release)],
        repo: &Repository,
        aliases: &impl AliasStore,
    ) -> Result<UnseedScope, String> {
        match select_release(candidates, Some("All releases".to_string()), repo, aliases)? {
            Some(id) => Ok(UnseedScope::One(id)),
            None => Ok(UnseedScope::All),
        }
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

    /// Render candidates as a leading-newline-indented list for
    /// embedding after "pass --release `<id>`" in the ambiguous-release
    /// error. Rows reuse the interactive picker's row format.
    pub(super) fn format_candidate_list(
        candidates: &[(ReleaseId, Release)],
        repo: &Repository,
        aliases: &impl AliasStore,
    ) -> String {
        let mut out = String::from(":");
        for (id, release) in candidates {
            out.push_str("\n  ");
            out.push_str(&format_candidate(id, release, repo, aliases));
        }
        out
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
            .into_iter()
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
            let ts = release.timestamp().as_secs();
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

    /// Pick a previously-added location URL from a list.
    ///
    /// Used by `location remove` to let the user choose which of their
    /// own added locations to retract. Errors if `no_input` is set,
    /// stdin is not a TTY, or the list is empty.
    pub fn pick_location(no_input: bool, urls: Vec<Url>) -> Result<Url, String> {
        if no_input || !std::io::stdin().is_terminal() {
            return Err(
                "interactive mode requires a terminal; pass <URL>, or use --no-input to disable"
                    .into(),
            );
        }
        if urls.is_empty() {
            return Err("no locations added by you for this artifact".into());
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

    /// Ask whether a directory registers as one collection artifact or as
    /// one artifact for each file directly inside it.
    ///
    /// Unlike the other prompts here this takes no `no_input`: the question
    /// has a back-compatible default, so `registration_plan` settles it and
    /// this is only reached on a terminal.
    pub fn pick_dir_mode(
        dir: &std::path::Path,
        labels: [String; 2],
    ) -> Result<super::DirMode, String> {
        let question = format!("Register {} as:", dir.display());
        let selection = inquire::Select::new(&question, labels.to_vec())
            .raw_prompt()
            .map_err(|e| format!("selection cancelled: {e}"))?;
        Ok(if selection.index == 0 {
            super::DirMode::Collection
        } else {
            super::DirMode::Each
        })
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
            let Ok(Some(name)) = maybe_name else { continue };
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
        tag_entries.sort_by_key(|a| std::cmp::Reverse(a.0));
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

/// Resolve either `--release <id>` or a `<revision>` arg to a single
/// [`ReleaseId`]. Clap enforces that exactly one is provided.
///
/// `--release <id>` always returns the named release if it exists,
/// regardless of who created it. `<revision>` narrows candidates to
/// releases authored by a delegate or by the local user; pass
/// `all_authors = true` to open the set up to everyone. After
/// filtering: zero candidates → `NoRelease`, one → auto-picked,
/// multiple → prompt (interactive) or `Ambiguous` error.
#[allow(clippy::too_many_arguments)]
fn resolve_target_release(
    release: Option<&str>,
    revision: Option<&str>,
    releases: &Releases<Repository>,
    repo: &Repository,
    delegates: &BTreeSet<Did>,
    local: &Did,
    all_authors: bool,
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
            let candidates: Vec<(ReleaseId, Release)> = collect_candidates(releases, oid)?
                .into_iter()
                .filter(|(_, r)| release_visible(r, delegates, local, all_authors))
                .collect();
            match candidates.as_slice() {
                [] => Err(error::Find::NoRelease(oid).into()),
                [(id, _)] => Ok(*id),
                _ => {
                    if no_input {
                        Err(error::Find::Ambiguous {
                            oid,
                            candidates: prompt::format_candidate_list(&candidates, repo, aliases),
                        }
                        .into())
                    } else {
                        prompt::pick_existing_release(&candidates, repo, aliases)
                            .map_err(error::ResolveTarget::Picker)
                    }
                }
            }
        }
        (None, None) => unreachable!("clap requires one of --release or <revision>"),
    }
}

/// Resolve `(oid, cid)` from `prompt::pick_interactive` to a single
/// [`ReleaseId`]. The picker merges artifacts across release COBs that
/// share `oid`; here we narrow to releases that pass the visibility
/// rule and contain the picked CID, then auto-pick if exactly one
/// remains, otherwise re-prompt at the release level.
/// `pick_interactive` already requires a TTY, so this helper assumes
/// interactive mode.
#[allow(clippy::too_many_arguments)]
fn resolve_release_after_pick(
    oid: Oid,
    cid: &Cid,
    releases: &Releases<Repository>,
    repo: &Repository,
    aliases: &impl AliasStore,
    delegates: &BTreeSet<Did>,
    local: &Did,
    all_authors: bool,
) -> Result<ReleaseId, error::Find> {
    let candidates: Vec<(ReleaseId, Release)> = collect_candidates(releases, oid)?
        .into_iter()
        .filter(|(_, r)| release_visible(r, delegates, local, all_authors))
        .filter(|(_, r)| r.artifact(cid).is_some())
        .collect();
    match candidates.as_slice() {
        [] => Err(error::Find::NoRelease(oid)),
        [(id, _)] => Ok(*id),
        _ => prompt::pick_existing_release(&candidates, repo, aliases).map_err(error::Find::Picker),
    }
}

/// Fetch all releases keyed by `oid`. Used by the ambiguous-resolution
/// paths to populate either an interactive picker or a rich error.
fn collect_candidates(
    releases: &Releases<Repository>,
    oid: Oid,
) -> Result<Vec<(ReleaseId, Release)>, error::Find> {
    releases
        .find_by_commit(oid)
        .map_err(|err| error::Find::Lookup { oid, err })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| error::Find::Lookup { oid, err })
}

/// Resolve a (possibly abbreviated) release-id string to a full
/// [`ReleaseId`]. The COB's first commit is a real git object, so
/// `git revparse_single` accepts short OIDs, full OIDs, and any other
/// ref name that points at the COB.
pub(crate) fn parse_release_id(s: &str, repo: &Repository) -> Result<ReleaseId, error::Resolve> {
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
    Stats(#[from] error::Stats),
    #[error(transparent)]
    Verify(#[from] error::Verify),
    #[error(transparent)]
    Locations(#[from] error::Locations),
    #[error(transparent)]
    Register(#[from] error::Register),
    #[error(transparent)]
    CreateRelease(#[from] error::CreateRelease),
    #[error(transparent)]
    Locate(#[from] error::Locate),
    #[error(transparent)]
    RemoveLocation(#[from] error::RemoveLocation),
    #[error(transparent)]
    Attest(#[from] error::Attest),
    #[error(transparent)]
    Redact(#[from] error::Redact),
    #[error(transparent)]
    Delete(#[from] error::Delete),
    #[error(transparent)]
    Metadata(#[from] error::Metadata),
    #[error(transparent)]
    Find(#[from] error::Find),
    #[error(transparent)]
    Resolve(#[from] error::Resolve),
    #[error(transparent)]
    Share(#[from] error::Share),
    #[error(transparent)]
    Node(#[from] node::Error),
    #[error(transparent)]
    Reconcile(#[from] reconcile::Error),
    #[error(transparent)]
    Watch(#[from] watch::Error),
}

impl RadArtifactError {
    /// Process exit code. Failing to run a command is `1`; only `verify`
    /// distinguishes a negative answer (`2`) from being unable to answer,
    /// because a caller acting on the result — an installer, or CI — has to
    /// tell "verified false" from "could not verify".
    fn exit_code(&self) -> i32 {
        match self {
            Self::Verify(err) => err.exit_code(),
            _ => 1,
        }
    }
}

mod command {
    use clap::Parser;
    use url::Url;

    use radicle_artifact::Cid;

    #[derive(Parser)]
    pub enum Command {
        /// Create a release for a commit (or reuse your own).
        Create(Create),
        // `add` is kept as a hidden alias for backward compatibility.
        #[clap(alias = "add")]
        Register(Register),
        /// Manage discovery locations for artifacts.
        Location(Location),
        Attest(Attest),
        Redact(Redact),
        Delete(Delete),
        /// Manage free-form metadata entries on artifacts.
        Metadata(Metadata),
        Show(Show),
        List(List),
        Stats(Stats),
        /// Check a local file against the artifacts registered in this repository.
        Verify(Verify),
        /// Locate a content identifier across every repository in local storage.
        Locate(Locate),
        /// Compute the BLAKE3 CID of a file or directory
        #[clap(name = "cid")]
        ComputeCid(ComputeCid),
        /// Fetch an artifact from a release COB into the local store
        Fetch(Fetch),
        /// Download an artifact from a release COB to disk
        Download(Download),
        /// Alias for `rad-artifact node seed`.
        Seed(Seed),
        /// Alias for `rad-artifact node unseed`.
        Unseed(Unseed),
        /// Reconcile release-COB locations with the artifacts in the node's store.
        ///
        /// Locations published in release COBs and the artifacts the local
        /// node is actually seeding can drift out of sync, e.g. the node
        /// is seeding a CID but no `radiroh://` location under our DID
        /// advertises it, or a location under our DID points at a CID the
        /// node no longer seeds, or at a previous endpoint id. This
        /// command inspects that drift, auto-adds missing locations, and
        /// (with the appropriate flag) removes the stale ones.
        Reconcile(crate::reconcile::Cli),
        /// Control the local rad-artifact seeder node.
        Node(crate::node::Cli),
        /// Seed trusted artifacts automatically as peers publish them.
        ///
        /// Runs until interrupted. Reacts to the radicle node's event
        /// stream, and sweeps every watched repository periodically so
        /// artifacts that landed while it was down are picked up too. An
        /// artifact is seeded when it passes the same trust rules as
        /// `verify`: registered by a delegate (or by you) in a release a
        /// delegate created, and redacted by nobody who counts.
        Watch(crate::watch::Cli),
    }

    /// Locate a content identifier across every repository in local storage.
    ///
    /// Prints, as JSON, every discovery location (repository, release,
    /// contributor, URL) that references the CID across all seeded repositories;
    /// with --releases, prints the releases that contain it instead. This
    /// search is node-wide, so --repo has no effect.
    #[derive(Parser)]
    pub struct Locate {
        /// The content identifier to locate.
        pub cid: Cid,
        /// Print the releases that contain the CID instead of their locations.
        #[clap(long)]
        pub releases: bool,
    }

    /// Manage discovery locations for artifacts.
    ///
    /// Locations record where an artifact can be retrieved from.
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

    /// Manage free-form metadata entries on artifacts.
    ///
    /// Only the artifact's author or a current repository delegate may
    /// set or remove metadata. Values are opaque strings; the keyspace is
    /// shared (last-writer-wins).
    #[derive(Parser)]
    pub struct Metadata {
        #[clap(subcommand)]
        pub command: MetadataCommand,
    }

    #[derive(Parser)]
    pub enum MetadataCommand {
        Set(MetadataSet),
        Unset(MetadataUnset),
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

    /// Fetch an artifact from a Radicle release COB into the local store.
    ///
    /// Pulls the bytes into the node's store without writing them to disk.
    /// Use `download` to also export to a file. With positional arguments,
    /// fetches a specific artifact directly; without arguments, interactively
    /// lists releases and artifacts to pick from.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact fetch

  Fetch by CID only (release looked up automatically):
    $ rad-artifact fetch --cid baf...abc

  Fetch a specific artifact into the store:
    $ rad-artifact fetch v1.0 --cid baf...abc

  Fetch and keep seeding it:
    $ rad-artifact fetch v1.0 --cid baf...abc --seed

  Fetch from a specific URL:
    $ rad-artifact fetch v1.0 --cid baf...abc --url https://example.com/my-binary")]
    pub struct Fetch {
        /// Git revision (commit, tag, or abbreviated OID). Required with --cid.
        #[clap(requires = "cid")]
        pub revision: Option<String>,
        /// Content identifier of the artifact to fetch.
        #[clap(long)]
        pub cid: Option<radicle_artifact::Cid>,
        /// Fetch from this URL directly, skipping the artifact's locations.
        #[clap(long)]
        pub url: Option<url::Url>,
        /// After fetching, keep seeding the artifact and add a
        /// `radiroh://` location under your DID so others can fetch it.
        #[clap(long)]
        pub seed: bool,
    }

    /// Download an artifact from a Radicle release COB to disk.
    ///
    /// Fetches the bytes into the store, then exports them to a file. With
    /// positional arguments, downloads a specific artifact directly; without
    /// arguments, interactively lists releases and artifacts to pick from.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact download

  Download by CID only (release looked up automatically):
    $ rad-artifact download --cid baf...abc

  Download a specific artifact:
    $ rad-artifact download v1.0 --cid baf...abc

  Download to a custom path:
    $ rad-artifact download v1.0 --cid baf...abc -o ./downloads/my-binary

  Download from a specific URL:
    $ rad-artifact download v1.0 --cid baf...abc --url https://example.com/my-binary

  Export bytes already in the store, without touching the network:
    $ rad-artifact download v1.0 --cid baf...abc --offline")]
    pub struct Download {
        /// Git revision (commit, tag, or abbreviated OID). Required with --cid.
        #[clap(requires = "cid")]
        pub revision: Option<String>,
        /// Content identifier of the artifact to download.
        #[clap(long)]
        pub cid: Option<radicle_artifact::Cid>,
        /// Output file path. Defaults to the artifact name in the current directory.
        #[clap(short, long)]
        pub output: Option<std::path::PathBuf>,
        /// Download from this URL directly, skipping the artifact's locations.
        #[clap(long)]
        pub url: Option<url::Url>,
        /// After downloading, keep seeding the artifact and add a
        /// `radiroh://` location under your DID so others can fetch it.
        #[clap(long)]
        pub seed: bool,
        /// Export from the local store only, never touching the network.
        /// Fails if the bytes aren't already complete in the store.
        #[clap(long, conflicts_with_all = ["url", "seed"])]
        pub offline: bool,
    }

    /// Alias for `rad-artifact node seed`.
    ///
    /// Computes the CID from the given path, asks the running node to
    /// register `seeded/{rid}/{cid}`, and writes a
    /// `radiroh://{endpoint_id}` location to the COB unless
    /// `--no-location`. Requires a running node — start one with
    /// `rad-artifact node start`.
    ///
    /// When multiple releases contain the same CID and `--release` is
    /// not given, the location is written to the most recently created
    /// matching release. Use `--release` to target a specific one, or
    /// prefer `register --seed` which resolves this automatically.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Seed an artifact registered in a release:
    $ rad-artifact seed ./my-binary

  Skip the COB write (e.g. for local sharing):
    $ rad-artifact seed ./my-binary --no-location

  Reference the file in place instead of copying bytes:
    $ rad-artifact seed ./my-binary --reference")]
    pub struct Seed {
        /// Path to the file or directory to seed.
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

    /// Alias for `rad-artifact node unseed`.
    ///
    /// Removes the `seeded/{rid}/{release}/{cid}` tag and retracts the
    /// `radiroh://` location under your DID for the given CID. With
    /// `--release`, both are restricted to a single release. Without it,
    /// a CID shared by several releases prompts you to pick one (or "All
    /// releases") at a terminal, and sweeps every release otherwise.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Stop seeding an artifact (prompts to scope when it spans releases):
    $ rad-artifact unseed --cid baf...abc

  Restrict the retraction to a specific release:
    $ rad-artifact unseed --cid baf...abc --release <release-id>")]
    pub struct Unseed {
        /// Content identifier of the artifact to stop seeding.
        #[clap(long)]
        pub cid: radicle_artifact::Cid,
        /// Target release id. When omitted, a CID in multiple releases
        /// prompts for one at a terminal, else sweeps every release.
        #[clap(long)]
        pub release: Option<String>,
    }

    /// Create a release for a commit (or reuse your own).
    ///
    /// Opens a release COB keyed to a commit so artifacts can be
    /// registered into it later with `register --release <id>`. This is
    /// the explicit form of the release that `register` would otherwise
    /// create on demand; create it once, then pass its id to many
    /// `register` calls.
    ///
    /// Idempotent for a single author: if you already created a release
    /// for the same commit and tag, its id is reused rather than
    /// minting a duplicate. Releases created by others are never reused,
    /// and a different tag on the same commit is treated as a distinct
    /// release.
    ///
    /// The revision is prompted interactively when omitted. Pass
    /// --no-input (or run without a TTY) to fail instead of hanging on a
    /// prompt.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Create a release for a tag and register artifacts into it:
    $ id=$(rad-artifact create v1.0)
    $ rad-artifact register ./bin-a --release \"$id\" -n bin-a
    $ rad-artifact register ./bin-b --release \"$id\" -n bin-b

  Machine-readable output:
    $ rad-artifact create v1.0 --json")]
    pub struct Create {
        /// Git revision (commit, tag, or abbreviated OID) for the release.
        pub revision: Option<String>,
        /// Emit the release id and commit as a JSON object on stdout.
        #[clap(long)]
        pub json: bool,
    }

    /// Register an artifact in a release, creating the release if needed.
    ///
    /// Records a signed artifact entry (CID + name) in the release COB,
    /// which is synced over the radicle protocol. Pass a local `<PATH>`
    /// to compute the CID from the file or directory contents, or use
    /// --cid to register a precomputed CID for an artifact you don't have
    /// locally. Exactly one of `<PATH>` or --cid must be provided.
    ///
    /// Registering from a `<PATH>` also records a `sizeBytes` metadata
    /// hint so peers can see the artifact's size before fetching; pass
    /// --no-size to skip it. Registering by --cid records no size.
    ///
    /// Registering records discovery metadata only; it does not hold or
    /// serve the bytes. To do that, seed the artifact with
    /// `rad-artifact seed <PATH>`.
    ///
    /// The release revision and artifact name are prompted interactively
    /// when not given. Pass --revision and -n/--name to skip prompts (or
    /// use --no-input in scripts to fail instead of hanging on a prompt).
    /// Use --release to target an existing release directly by its id
    /// (skipping all commit/tag resolution).
    ///
    /// A directory can register in two ways: as one collection artifact
    /// for the whole tree (--collection), or as one artifact for each
    /// file directly inside it (--each, which does not recurse). With
    /// neither flag, the CLI asks. With --no-input, or with -n/--name,
    /// a directory registers as one collection.
    #[derive(Parser)]
    #[clap(
        group = clap::ArgGroup::new("source").required(true).args(["path", "cid"]),
        group = clap::ArgGroup::new("dir_mode").args(["collection", "each"]),
        after_long_help = "\
Examples:
  Interactive: compute CID from a file, pick commit/tag, prompt for name:
    $ rad-artifact register ./my-binary

  Fully non-interactive:
    $ rad-artifact register ./my-binary --revision v1.0 --name \"my-binary v1.0\"

  Register and seed in one step (reuses the computed CID; needs a running node):
    $ rad-artifact register ./my-binary --revision v1.0 --name \"my-binary v1.0\" --seed

  Target an existing release by id (no commit/tag resolution):
    $ rad-artifact register ./my-binary --release <release-id> --name \"my-binary v1.0\"

  Register a precomputed CID without local bytes:
    $ rad-artifact register --cid baf...abc --revision v1.0 --name \"my-binary v1.0\"

  Register every binary in a build directory as its own artifact:
    $ rad-artifact register ./target/release --each --revision v1.0"
    )]
    pub struct Register {
        /// Path to the local file or directory to register.
        ///
        /// The CID is computed from the contents: files use the raw codec
        /// (0x55), directories use the blake3-hashseq codec (0x80).
        pub path: Option<std::path::PathBuf>,
        /// Precomputed CID. Use when the artifact bytes aren't available
        /// locally. Conflicts with `<PATH>`.
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
        /// After registering, also seed the artifact via the local node
        /// and add a `radiroh://` location for it — the same as a
        /// follow-up `rad-artifact seed <PATH>`, but reusing the CID
        /// already computed here (one hash pass, one command). Requires a
        /// local `<PATH>` and a running node; conflicts with --cid.
        #[clap(long, conflicts_with = "cid")]
        pub seed: bool,
        /// Emit `{cid, name, releaseId, oid, metadata}` as JSON on stdout
        /// instead of the human-readable summary, so scripts can capture the
        /// release id and CID without scraping stderr. `metadata` carries the
        /// `sizeBytes` hint when recorded, and is empty otherwise. One object
        /// per line, so --each emits one line per registered file.
        #[clap(long)]
        pub json: bool,
        /// Register a directory as one collection artifact: a single CID
        /// covering the whole tree. This is what a directory does by
        /// default, so the flag only skips the question.
        #[clap(long, conflicts_with = "cid")]
        pub collection: bool,
        /// Register each file directly inside a directory as its own
        /// artifact, named by its file name. Does not recurse into
        /// subdirectories. Conflicts with -n/--name, because each
        /// artifact takes its own name.
        #[clap(long, conflicts_with_all = ["cid", "name"])]
        pub each: bool,
        /// Skip recording the `sizeBytes` metadata hint. By default,
        /// registering from a local `<PATH>` records the artifact's byte
        /// size; with --cid (no local bytes) no size is recorded regardless.
        #[clap(long)]
        pub no_size: bool,
        /// Also consider releases authored by users who are not
        /// repository delegates (and not the local user) when matching
        /// a `<revision>`. By default only delegate-authored or
        /// local-authored releases are eligible to attach to.
        #[clap(long)]
        pub all_authors: bool,
    }

    /// Add a download location URL for an artifact CID
    ///
    /// Records where an artifact can be retrieved from.
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

  Add an HTTPS download location:
    $ rad-artifact location add --revision v1.0 --cid baf...abc https://example.com/my-binary

  Add an iroh-blobs endpoint:
    $ rad-artifact location add --revision v1.0 --cid baf...abc radiroh://<endpoint-id>

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
        /// Also consider releases authored by users who are not
        /// repository delegates (and not the local user) when matching
        /// a `<revision>`.
        #[clap(long)]
        pub all_authors: bool,
    }

    /// Attest that you verified an artifact CID
    ///
    /// Records that the signing node built from the same commit and
    /// obtained the same CID. Idempotent — attesting twice is a no-op.
    ///
    /// Without arguments, interactively lists releases and artifacts to
    /// pick from. Pass both `<REVISION>` and --cid to skip the prompts.
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
        /// with --cid unless `<revision>` is given.
        #[clap(long, requires = "cid")]
        pub release: Option<String>,
        /// Content identifier for the artifact to attest. Required with
        /// a target (`<revision>` or --release).
        #[clap(long, requires = "target")]
        pub cid: Option<Cid>,
        /// Also consider releases authored by users who are not
        /// repository delegates (and not the local user) when matching
        /// a `<revision>`.
        #[clap(long)]
        pub all_authors: bool,
    }

    /// Remove your ref to a release
    ///
    /// The release disappears once no user has a ref to it. Your actions
    /// stay visible if another user's actions build on them. Peers that
    /// already fetched your actions can keep them. To withdraw a published
    /// artifact, use `redact` instead.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Delete a release by its id:
    $ rad-artifact delete 3f2a9c1")]
    pub struct Delete {
        /// Id of the release to delete.
        #[clap(value_name = "RELEASE_ID")]
        pub release: String,
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
    /// pick from and prompts for a reason. Pass both `<REVISION>` and --cid
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
        /// with --cid unless `<revision>` is given.
        #[clap(long, requires = "cid")]
        pub release: Option<String>,
        /// Content identifier for the artifact to redact. Required with
        /// a target (`<revision>` or --release).
        #[clap(long, requires = "target")]
        pub cid: Option<Cid>,
        /// Reason for the redaction.
        #[clap(short = 'm', long = "reason")]
        pub reason: Option<String>,
        /// Also consider releases authored by users who are not
        /// repository delegates (and not the local user) when matching
        /// a `<revision>`.
        #[clap(long)]
        pub all_authors: bool,
    }

    /// Set or overwrite a metadata entry on an artifact.
    ///
    /// Only the artifact's author or a current repository delegate can
    /// set metadata. By default the value is stored as a JSON string;
    /// pass --json to parse `<VALUE>` as JSON instead. The keyspace is
    /// shared across contributors (last-writer-wins).
    ///
    /// Without --revision/--release and --cid, interactively lists
    /// releases and artifacts to pick from.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode (pick from available releases):
    $ rad-artifact metadata set build-env \"nix --pure\"

  Set metadata on a specific artifact:
    $ rad-artifact metadata set --revision v1.0 --cid baf...abc build-env \"nix --pure\"

  Store a structured JSON value:
    $ rad-artifact metadata set --json reproducible true
    $ rad-artifact metadata set --json sbom '{\"format\":\"cyclonedx\",\"url\":\"https://...\"}'")]
    #[clap(group = clap::ArgGroup::new("target").args(["revision", "release"]))]
    pub struct MetadataSet {
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
        /// Parse `<VALUE>` as JSON. Without this flag the value is stored
        /// as a JSON string.
        #[clap(long)]
        pub json: bool,
        /// Metadata key.
        pub key: String,
        /// Value to store. Treated as a string unless --json is set.
        pub value: String,
        /// Also consider releases authored by users who are not
        /// repository delegates (and not the local user) when matching
        /// a `<revision>`.
        #[clap(long)]
        pub all_authors: bool,
    }

    /// Remove a metadata entry from an artifact.
    ///
    /// Only the artifact's author or a current repository delegate can
    /// remove metadata. Any authorized contributor may remove any key.
    ///
    /// Without --revision/--release and --cid, interactively lists
    /// releases and artifacts to pick from.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Interactive mode:
    $ rad-artifact metadata unset build-env

  Remove a specific entry:
    $ rad-artifact metadata unset --revision v1.0 --cid baf...abc build-env")]
    #[clap(group = clap::ArgGroup::new("target").args(["revision", "release"]))]
    pub struct MetadataUnset {
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
        /// Metadata key to remove.
        pub key: String,
        /// Also consider releases authored by users who are not
        /// repository delegates (and not the local user) when matching
        /// a `<revision>`.
        #[clap(long)]
        pub all_authors: bool,
    }

    /// Remove a download location for an artifact.
    ///
    /// Retracts a previously added location.
    ///
    /// Without arguments, interactively lists releases and artifacts to
    /// pick from, then prompts for the URL to remove from the locations
    /// you previously added. Pass --revision/--release and --cid
    /// (and optionally `<URL>`) to skip the prompts.
    #[derive(Parser)]
    #[clap(
        group = clap::ArgGroup::new("target").args(["revision", "release"]),
        after_long_help = "\
Examples:
  Interactive mode (pick from your added locations):
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
        /// Also consider releases authored by users who are not
        /// repository delegates (and not the local user) when matching
        /// a `<revision>`.
        #[clap(long)]
        pub all_authors: bool,
    }

    /// Show the release COB for a Git commit or annotated tag.
    ///
    /// By default only releases (and artifacts within them) authored by
    /// a repository delegate or by the local user are shown. Pass
    /// `--all-authors` to include releases and artifacts from other
    /// users. Pass `--release <id>` to target a specific release
    /// directly when multiple exist for the same commit.
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
        /// disambiguation. Conflicts with `<revision>`.
        #[clap(long)]
        pub release: Option<String>,
    }

    /// List all release COBs for a repository.
    ///
    /// By default only releases (and artifacts within them) authored by
    /// a repository delegate or by the local user are shown. Pass
    /// `--all-authors` to include releases and artifacts from other users.
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

    /// Report repository-wide artifact statistics.
    ///
    /// Today it reports release counts, split by whether a repository
    /// delegate created the release and whether it still has an artifact
    /// to show. A release is hidden when every artifact in it has been
    /// redacted by a trusted party, or when it has no artifacts at all.
    ///
    /// Unlike a ref walk, this materializes every release, so it is
    /// served from the cache where one is available.
    ///
    /// Further figures will be added as fields, so a script reading the
    /// JSON keeps working.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Show the counts for the current repository:
    $ rad-artifact stats

  Machine-readable figures:
    $ rad-artifact stats --json")]
    pub struct Stats {
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
    }

    /// Check a local file against the artifacts registered in this
    /// repository's releases.
    ///
    /// Hashes `<PATH>` and looks for an artifact with that CID in any
    /// release. Verification succeeds when a repository delegate (or you)
    /// registered these exact bytes and nobody trusted has redacted them.
    ///
    /// Reads only local storage: no daemon, no network, and no signer. The
    /// repository must already be replicated locally.
    ///
    /// Exits 0 when verified, 2 when the bytes are not trustworthy, and 1
    /// when the check could not be made at all.
    #[derive(Parser)]
    #[clap(after_long_help = "\
Examples:
  Verify a downloaded release binary:
    $ rad-artifact verify ./rad-artifact_0.18.0_aarch64-apple-darwin

  Verify against a repository you are not currently in:
    $ rad-artifact --repository rad:z4VYyJ9KuwMNkXGQnmKuGPGKw3inv verify ./my-binary

  Accept artifacts registered by users who are not delegates:
    $ rad-artifact verify ./my-binary --all-authors

  Machine-readable result:
    $ rad-artifact verify ./my-binary --json")]
    pub struct Verify {
        /// Path to the file or directory to check.
        pub path: std::path::PathBuf,
        /// Also accept artifacts registered by users who are not
        /// repository delegates.
        #[clap(long)]
        pub all_authors: bool,
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
    }
}

mod error {
    use radicle::{node, rad::CwdError, storage::RepositoryError};
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
        #[error("failed to list releases, could not serialize to JSON")]
        Json(#[source] serde_json::Error),
    }

    #[derive(Debug, Error)]
    pub enum Stats {
        #[error("failed to count releases")]
        Counts(#[source] cob::store::Error),
        #[error("failed to report stats, could not serialize to JSON")]
        Json(#[source] serde_json::Error),
    }

    /// Hashing a path into a CID. Shared by `register` and `verify` so both
    /// report the same failure for the same path.
    #[derive(Debug, Error)]
    pub enum ComputeCid {
        #[error("failed to compute CID from path")]
        Io(#[source] std::io::Error),
        #[error(transparent)]
        Protocol(radicle_artifact_core::Error),
    }

    /// A `Cid` is ~100 bytes, which pushes any enum holding one past
    /// clippy's `result_large_err` limit. These errors only ever format the
    /// CID, so they keep its canonical string form instead.
    #[derive(Debug, Error)]
    pub enum Verify {
        #[error(transparent)]
        ComputeCid(#[from] ComputeCid),
        #[error("failed to look up CID {cid}")]
        Lookup {
            cid: String,
            #[source]
            err: Box<cob::store::Error>,
        },
        #[error("no artifact with CID {cid} is registered in any release of {rid}")]
        NoMatch { cid: String, rid: RepoId },
        #[error(
            "artifact {cid} has been redacted by a trusted party:\n{}",
            display_redactions(redactions)
        )]
        Redacted {
            cid: String,
            redactions: std::collections::BTreeMap<Did, String>,
        },
        #[error("artifact {cid} is only registered by {author}, who is not a repository delegate\n  hint: pass --all-authors to accept it anyway")]
        UntrustedAuthor { cid: String, author: Did },
        #[error("failed to serialize verify output to JSON")]
        Json(#[source] serde_json::Error),
    }

    impl Verify {
        /// `2` when the check ran and the bytes are not trustworthy, `1` when
        /// the check could not be made at all. Callers such as an installer
        /// need to tell "verified false" from "could not verify".
        pub fn exit_code(&self) -> i32 {
            match self {
                Self::NoMatch { .. } | Self::Redacted { .. } | Self::UntrustedAuthor { .. } => 2,
                Self::ComputeCid(_) | Self::Lookup { .. } | Self::Json(_) => 1,
            }
        }
    }

    fn display_redactions(redactions: &std::collections::BTreeMap<Did, String>) -> String {
        redactions
            .iter()
            .map(|(did, reason)| format!("  {did}: {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[derive(Debug, Error)]
    pub enum Locations {
        #[error("failed to enumerate repositories")]
        Storage(#[source] radicle::storage::Error),
        #[error("failed to serialize locate output to JSON")]
        Json(#[source] serde_json::Error),
    }

    #[derive(Debug, Error)]
    pub enum CreateRelease {
        #[error("{0}")]
        Usage(String),
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("commit {oid} has {} release(s) you authored; re-run in a terminal to pick one, or name its tag (e.g. `create <tag>`) if it has one (candidates: {})", candidates.len(), display_ids(candidates))]
        Ambiguous {
            oid: Oid,
            candidates: Vec<ReleaseId>,
        },
        #[error("failed to create release for commit {oid}")]
        Create {
            oid: Oid,
            #[source]
            err: radicle_artifact::error::Create,
        },
        #[error("failed to serialize create output to JSON")]
        Json(#[source] serde_json::Error),
    }

    #[derive(Debug, Error)]
    pub enum Register {
        #[error("{0}")]
        Usage(String),
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
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
        #[error(transparent)]
        ComputeCid(#[from] ComputeCid),
        #[error("failed to read size from path")]
        Io(#[source] std::io::Error),
        #[error("failed to serialize register output to JSON")]
        Json(#[source] serde_json::Error),
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
    pub enum Delete {
        #[error(transparent)]
        Resolve(#[from] Resolve),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("failed to delete release {id}")]
        Store {
            id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
    }

    #[derive(Debug, Error)]
    pub enum Metadata {
        #[error("{0}")]
        Usage(String),
        #[error(transparent)]
        ResolveTarget(#[from] ResolveTarget),
        #[error(transparent)]
        Find(#[from] Find),
        #[error("artifact {cid} not found in release {id}")]
        UnknownCid { id: ReleaseId, cid: Cid },
        #[error("not authorized to manage metadata on artifact {cid}: only the artifact author ({artifact_author}) or a repository delegate may. local DID is {local}")]
        NotAuthorized {
            local: Box<Did>,
            artifact_author: Box<Did>,
            cid: Box<Cid>,
        },
        #[error("--json was set but value is not valid JSON: {err}")]
        InvalidJson {
            #[source]
            err: serde_json::Error,
        },
        #[error(transparent)]
        InvalidKey(radicle_artifact::error::Metadata),
        #[error("failed to update metadata on release {id}")]
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
        #[error("multiple releases found for the commit {oid}, pass --release <id>{candidates}")]
        Ambiguous { oid: Oid, candidates: String },
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
        #[error("{0}")]
        Picker(String),
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

    #[derive(Debug, Error)]
    #[error(
        "could not resolve '{revision}' to a git object\n  hint: if the tag or commit is local, push it to Radicle storage with `git push rad --tags`"
    )]
    pub struct Resolve {
        pub revision: String,
        #[source]
        pub err: radicle::git::raw::Error,
    }

    #[derive(Debug, Error)]
    pub enum Repository {
        #[error(
            "failed to find Radicle repository for current working directory; \
             use --repo <RID> to target a repository in storage"
        )]
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
    #[error("failed to announce changes")]
    pub struct Announce(#[source] pub node::Error);

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
        // usable source has been added. Surface the actionable recovery
        // paths so the user doesn't get a generic "no locations" error.
        #[error("no download locations known for artifact {cid}\n  hint: pass --url <URL> to fetch directly, or ask a seeder to run `rad-artifact seed`")]
        NoLocationsForCid { cid: radicle_artifact::Cid },
        #[error(transparent)]
        Protocol(radicle_artifact_core::Error),
        #[error("I/O error")]
        Io(#[source] std::io::Error),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use radicle::crypto::PublicKey;
    use radicle::identity::Did;
    use radicle_artifact_core::cid::{blake3_hash_to_cid, ArtifactKind};

    use super::{
        dir_mode_labels, each_rows, error, registration_plan, rejection, top_level_members,
        verify_failure, Cid, Pending, Plan, Skipped, Untrusted,
    };

    /// Distinct DIDs keyed by a single byte, so a test can name its
    /// participants. Never used to verify a signature, so the bytes don't
    /// have to be a valid curve point.
    fn did(n: u8) -> Did {
        Did::from(PublicKey::from_bytes([n; 32]))
    }

    /// Production-faithful CID: BLAKE3 multihash, raw codec.
    fn cid() -> Cid {
        blake3_hash_to_cid(blake3::hash(b"artifact"), ArtifactKind::Blob)
    }

    const DELEGATE: u8 = 1;
    const STRANGER: u8 = 4;

    #[test]
    fn redaction_outranks_untrusted_author() {
        let rid = radicle::prelude::RepoId::from_urn("rad:z4VYyJ9KuwMNkXGQnmKuGPGKw3inv").unwrap();
        let redactions = BTreeMap::from([(did(DELEGATE), "compromised".to_owned())]);
        let reasons = [
            Untrusted::Author(did(STRANGER)),
            Untrusted::Redacted(redactions),
        ];
        // The stranger comes first, but the redaction is what the user
        // needs to see.
        assert_eq!(rejection(&reasons, &cid(), rid).exit_code(), 2);
        assert!(matches!(
            rejection(&reasons, &cid(), rid),
            error::Verify::Redacted { .. }
        ));
    }

    #[test]
    fn no_candidates_reports_no_match() {
        let rid = radicle::prelude::RepoId::from_urn("rad:z4VYyJ9KuwMNkXGQnmKuGPGKw3inv").unwrap();
        assert!(matches!(
            rejection(&[], &cid(), rid),
            error::Verify::NoMatch { .. }
        ));
    }

    #[test]
    fn negative_answers_exit_2_and_failures_exit_1() {
        let rid = radicle::prelude::RepoId::from_urn("rad:z4VYyJ9KuwMNkXGQnmKuGPGKw3inv").unwrap();
        // A caller acting on the result has to tell "verified false" from
        // "could not verify".
        assert_eq!(rejection(&[], &cid(), rid).exit_code(), 2);
        assert_eq!(
            rejection(&[Untrusted::Author(did(STRANGER))], &cid(), rid).exit_code(),
            2
        );
        assert_eq!(
            error::Verify::from(error::ComputeCid::Io(std::io::Error::other("unreadable")))
                .exit_code(),
            1
        );
    }

    /// `--json` has to answer with data, not only an exit code — but only
    /// when there is a verdict to report.
    #[test]
    fn a_verdict_gets_a_json_payload_and_a_failure_does_not() {
        let rid = radicle::prelude::RepoId::from_urn("rad:z4VYyJ9KuwMNkXGQnmKuGPGKw3inv").unwrap();
        let verdict = verify_failure(cid(), &rejection(&[], &cid(), rid)).unwrap();
        let json = serde_json::to_value(&verdict).unwrap();
        assert_eq!(json["verified"], serde_json::json!(false));
        assert_eq!(json["reason"], serde_json::json!("noMatch"));

        let redacted = rejection(
            &[Untrusted::Redacted(BTreeMap::from([(
                did(DELEGATE),
                "compromised".to_owned(),
            )]))],
            &cid(),
            rid,
        );
        let json = serde_json::to_value(verify_failure(cid(), &redacted).unwrap()).unwrap();
        assert_eq!(json["reason"], serde_json::json!("redacted"));
        assert_eq!(json["redactions"][did(DELEGATE).to_string()], "compromised");

        // "Could not verify" is not a verdict, so it carries no payload.
        let failed =
            error::Verify::from(error::ComputeCid::Io(std::io::Error::other("unreadable")));
        assert!(verify_failure(cid(), &failed).is_none());
    }

    // -- `register <DIR>` planning --

    /// `registration_plan` with the terminal available, which is the only
    /// state where the directory question can be asked.
    fn plan(is_dir: bool, collection: bool, each: bool, named: bool) -> Result<Plan, String> {
        registration_plan(is_dir, collection, each, named, true)
    }

    #[test]
    fn file_registers_as_one_blob() {
        assert_eq!(plan(false, false, false, false), Ok(Plan::Blob));
    }

    #[test]
    fn directory_prompts_in_a_terminal() {
        assert_eq!(plan(true, false, false, false), Ok(Plan::Ask));
    }

    /// The contract with every script written before --each existed: with
    /// --no-input, or with a piped stdin, a directory is still one collection.
    #[test]
    fn no_input_directory_stays_a_collection() {
        assert_eq!(
            registration_plan(true, false, false, false, false),
            Ok(Plan::Collection)
        );
    }

    /// One name means one artifact, so there is nothing to ask.
    #[test]
    fn named_directory_skips_the_question() {
        assert_eq!(plan(true, false, false, true), Ok(Plan::Collection));
    }

    #[test]
    fn flags_override_the_question() {
        assert_eq!(plan(true, false, true, false), Ok(Plan::Each));
        assert_eq!(plan(true, true, false, false), Ok(Plan::Collection));
    }

    #[test]
    fn directory_flags_are_rejected_for_a_file() {
        assert!(plan(false, true, false, false).is_err());
        assert!(plan(false, false, true, false).is_err());
    }

    #[test]
    fn dir_mode_labels_use_the_singular_for_one_file() {
        let [collection, each] = dir_mode_labels(1, 1024, 1);
        assert_eq!(collection, "one collection artifact (1 file, 1.0 KiB)");
        assert_eq!(each, "1 separate artifact (top-level files only)");
    }

    /// The two counts differ on purpose: a collection covers the whole tree,
    /// --each covers one level.
    #[test]
    fn dir_mode_labels_count_the_tree_and_the_top_level_apart() {
        let [collection, each] = dir_mode_labels(5, 1024, 3);
        assert_eq!(collection, "one collection artifact (5 files, 1.0 KiB)");
        assert_eq!(each, "3 separate artifacts (top-level files only)");
    }

    // -- `--each` summary rows --

    fn pending(name: &str, size: Option<u64>) -> Pending {
        Pending {
            cid: cid(),
            name: name.to_string(),
            size,
            path: None,
        }
    }

    #[test]
    fn each_rows_align_cids() {
        let rows = each_rows(&[
            pending("a.bin", Some(1024)),
            pending("longer.bin", Some(2048)),
        ]);
        let full = cid().to_string();
        assert_eq!(rows[0], format!("  a.bin       {full}  1.0 KiB"));
        assert_eq!(rows[1], format!("  longer.bin  {full}  2.0 KiB"));
    }

    #[test]
    fn each_rows_omit_missing_sizes() {
        let rows = each_rows(&[pending("a.bin", None)]);
        assert_eq!(rows[0], format!("  a.bin  {}", cid()));
    }

    // -- top-level directory walk --

    /// A directory holding `files` at the top level and `dirs` empty
    /// subdirectories.
    fn dir_with(files: &[&str], dirs: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        for name in files {
            std::fs::write(dir.path().join(name), b"data").unwrap();
        }
        for name in dirs {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        dir
    }

    #[test]
    fn top_level_members_skip_subdirectories() {
        let dir = dir_with(&["a.bin"], &["sub", "other"]);
        let (members, skipped) = top_level_members(dir.path()).unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "a.bin");
        assert_eq!(members[0].bytes, 4);
        assert_eq!(skipped.dirs, 2);
    }

    #[test]
    fn top_level_members_are_sorted_by_name() {
        let dir = dir_with(&["c.bin", "a.bin", "b.bin"], &[]);
        let (members, _) = top_level_members(dir.path()).unwrap();

        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a.bin", "b.bin", "c.bin"]);
    }

    /// A tree whose files all sit in subdirectories has nothing for --each to
    /// register; the caller turns this into a collection or a usage error.
    #[test]
    fn top_level_members_is_empty_for_a_nested_only_directory() {
        let dir = dir_with(&[], &["sub"]);
        std::fs::write(dir.path().join("sub/deep.bin"), b"data").unwrap();
        let (members, skipped) = top_level_members(dir.path()).unwrap();

        assert!(members.is_empty());
        assert_eq!(skipped.dirs, 1);
    }

    /// A symlink is not a regular file, so --each cannot register it. It
    /// still has to be counted, or it disappears without a word.
    #[cfg(unix)]
    #[test]
    fn top_level_members_count_what_they_cannot_register() {
        let dir = dir_with(&["a.bin"], &[]);
        std::os::unix::fs::symlink(dir.path().join("a.bin"), dir.path().join("link.bin")).unwrap();
        let (members, skipped) = top_level_members(dir.path()).unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(skipped.other, 1);
    }

    #[test]
    fn skipped_notes_one_per_reason() {
        assert!(Skipped::default().notes().is_empty());

        let notes = Skipped { dirs: 1, other: 2 }.notes();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("1 subdirectory"), "{}", notes[0]);
        assert!(notes[1].contains("2 entries"), "{}", notes[1]);
    }
}
