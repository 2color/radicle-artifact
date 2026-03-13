//! A program to create and inspect artifact release COBs for a repository.
//!
//! Run `rad-artifact --help` to see how to use the program.

use std::{collections::BTreeSet, error::Error as _, time::Duration};

use clap::Parser;

use radicle::{
    cob, crypto,
    crypto::signature::Signer,
    git::Oid,
    node::{
        device::Device,
        sync::{Announcer, AnnouncerConfig, ReplicationFactor},
        Handle, Node,
    },
    prelude::{Profile, ReadStorage, RepoId},
    profile,
    storage::git::Repository,
};
use radicle_artifact::*;

const TIMEOUT: Duration = Duration::from_millis(5000);

fn main() {
    if let Err(err) = fallible_main() {
        eprintln!("ERROR: {err}");
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
/// Output is JSON by default, or human-readable with --pretty.
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

    let (synced, unsynced) = node
        .seeds(repo_id)
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

    node.announce(repo_id, TIMEOUT, announcer, |_, _| ())
        .map_err(error::Announce::Announcement)?;

    Ok(())
}

fn run(args: Args) -> Result<(), RadArtifactError> {
    use command::*;

    let profile = load_profile()?;
    let repo = args.repository(&profile)?;
    let mut releases = open_releases(&repo)?;
    match args.command {
        Command::Create(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            create_release(cmd, &mut releases, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Add(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            add_artifact(cmd, &mut releases, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Locate(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            locate_artifact(cmd, &mut releases, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::RemoveLocation(cmd) => {
            let signer = profile.signer().map_err(error::Signer)?;
            remove_location(cmd, &mut releases, &signer)?;
            if !args.no_sync {
                announce(&profile, repo.id)?;
            }
        }
        Command::Show(cmd) => show_release(cmd, &releases)?,
        Command::List(cmd) => list_releases(cmd, &releases)?,
    }

    Ok(())
}

fn create_release<G>(
    command::Create { oid }: command::Create,
    releases: &mut Releases<Repository>,
    signer: &Device<G>,
) -> Result<(), error::Create>
where
    G: Signer<crypto::Signature>,
{
    let release = releases
        .create(oid, signer)
        .map_err(|err| error::Create { oid, err })?;
    println!("{}", release.id());
    Ok(())
}

fn add_artifact<G>(
    command::Add { oid, cid, name }: command::Add,
    releases: &mut Releases<Repository>,
    signer: &Device<G>,
) -> Result<(), error::Add>
where
    G: Signer<crypto::Signature>,
{
    let id = find_unique_by_oid(oid, releases)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Add::Store { id, err })?;
    release
        .add_artifact(cid.clone(), name, signer)
        .map_err(|err| error::Add::Store { id, err })?;
    println!("{cid}");
    Ok(())
}

fn locate_artifact<G>(
    command::Locate { oid, cid, url }: command::Locate,
    releases: &mut Releases<Repository>,
    signer: &Device<G>,
) -> Result<(), error::Locate>
where
    G: Signer<crypto::Signature>,
{
    let id = find_unique_by_oid(oid, releases)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::Locate::Store { id, err })?;
    release
        .add_location(cid, url, signer)
        .map_err(|err| error::Locate::Store { id, err })?;
    Ok(())
}

fn remove_location<G>(
    command::RemoveLocation { oid, cid, url }: command::RemoveLocation,
    releases: &mut Releases<Repository>,
    signer: &Device<G>,
) -> Result<(), error::RemoveLocation>
where
    G: Signer<crypto::Signature>,
{
    let id = find_unique_by_oid(oid, releases)?;
    let mut release = releases
        .get_mut(&id)
        .map_err(|err| error::RemoveLocation::Store { id, err })?;
    release
        .remove_location(cid, url, signer)
        .map_err(|err| error::RemoveLocation::Store { id, err })?;
    Ok(())
}

fn show_release(
    command::Show { pretty, oid }: command::Show,
    releases: &Releases<Repository>,
) -> Result<(), error::Show> {
    let id = find_unique_by_oid(oid, releases)?;
    let release = releases
        .get(&id)
        .map_err(|err| error::Find::FindOid { oid, err })?
        .ok_or(error::Find::NoRelease(oid))?;
    let show = radicle_artifact::display::Release::new(id, &release);
    if pretty {
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
    command::List { pretty, verbose }: command::List,
    releases: &Releases<Repository>,
) -> Result<(), error::List> {
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
    let releases = display::Releases::new(iter);
    if pretty {
        println!("{}", releases.pretty());
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&releases).map_err(error::List::Json)?
        );
    }
    Ok(())
}

/// Find the unique release for a given OID. Errors if none or more than one exist.
fn find_unique_by_oid(
    oid: Oid,
    releases: &Releases<Repository>,
) -> Result<ReleaseId, error::Find> {
    let mut iter = releases
        .find_by_oid(oid)
        .map_err(|err| error::Find::FindOid { oid, err })?;
    let (id, _release) = iter
        .next()
        .ok_or(error::Find::NoRelease(oid))?
        .map_err(|err| error::Find::FindOid { oid, err })?;

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
    Create(#[from] error::Create),
    #[error(transparent)]
    List(#[from] error::List),
    #[error(transparent)]
    Add(#[from] error::Add),
    #[error(transparent)]
    Locate(#[from] error::Locate),
    #[error(transparent)]
    RemoveLocation(#[from] error::RemoveLocation),
}

mod command {
    use clap::Parser;
    use radicle::git::Oid;
    use url::Url;

    use radicle_artifact::Cid;

    #[derive(Parser)]
    pub enum Command {
        Create(Create),
        Add(Add),
        Locate(Locate),
        RemoveLocation(RemoveLocation),
        Show(Show),
        List(List),
    }

    /// Create a release COB for a specific Git commit or annotated tag.
    ///
    /// Write the release ID to the standard output.
    #[derive(Parser)]
    pub struct Create {
        /// Git object id for the commit or annotated tag.
        pub oid: Oid,
    }

    /// Add an artifact to an existing release.
    ///
    /// The artifact is identified by its content identifier (CID).
    #[derive(Parser)]
    pub struct Add {
        /// Git object id the release is associated with.
        pub oid: Oid,
        /// Content identifier for the artifact.
        pub cid: Cid,
        /// Human-readable description of the artifact.
        pub name: String,
    }

    /// Add a discovery location for an artifact.
    ///
    /// Announces where an artifact can be retrieved from.
    #[derive(Parser)]
    pub struct Locate {
        /// Git object id the release is associated with.
        pub oid: Oid,
        /// Content identifier for the artifact.
        pub cid: Cid,
        /// URL where the artifact can be retrieved.
        pub url: Url,
    }

    /// Remove a discovery location for an artifact.
    ///
    /// Retracts a previously announced location.
    #[derive(Parser)]
    pub struct RemoveLocation {
        /// Git object id the release is associated with.
        pub oid: Oid,
        /// Content identifier for the artifact.
        pub cid: Cid,
        /// URL to remove.
        pub url: Url,
    }

    /// Show the release COB for a Git commit or annotated tag.
    #[derive(Parser)]
    pub struct Show {
        /// Format output in a more human oriented way than JSON.
        #[clap(long)]
        pub pretty: bool,
        /// Git object id the release is associated with.
        pub oid: Oid,
    }

    /// List all release COBs for a repository.
    #[derive(Parser)]
    pub struct List {
        /// Format output in a more human oriented way than JSON.
        #[clap(long)]
        pub pretty: bool,
        /// Output all information, including intermediate errors.
        #[clap(long, short)]
        pub verbose: bool,
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
        Find(#[from] Find),
        #[error("failed to show release, could not serialize to JSON")]
        Json(#[source] serde_json::Error),
    }

    #[derive(Debug, Error)]
    #[error("failed to create a new release for the commit {oid}")]
    pub struct Create {
        pub oid: Oid,
        #[source]
        pub err: cob::store::Error,
    }

    #[derive(Debug, Error)]
    pub enum List {
        #[error("failed to list releases")]
        All(#[source] cob::store::Error),
        #[error("failed to list releases, could not serialize to JSON")]
        Json(#[source] serde_json::Error),
    }

    #[derive(Debug, Error)]
    pub enum Add {
        #[error(transparent)]
        Find(#[from] Find),
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
        Find(#[from] Find),
        #[error("failed to add location to release {id}")]
        Store {
            id: ReleaseId,
            #[source]
            err: cob::store::Error,
        },
    }

    #[derive(Debug, Error)]
    pub enum RemoveLocation {
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
        FindOid {
            oid: Oid,
            #[source]
            err: cob::store::Error,
        },
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
}