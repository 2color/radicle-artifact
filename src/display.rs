//! Display forms of Release data.
//!
//! These can be used in tools that wish to display data, such as the
//! `rad-artifact` CLI tool.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use radicle::{git::Oid, identity::Did, node::AliasStore, storage::git::Repository};
use serde::Serialize;
use url::Url;

use crate::ReleaseId;

/// Resolve a DID's alias via an [`AliasStore`], returning the string if found.
fn resolve(did: &Did, aliases: &impl AliasStore) -> Option<String> {
    aliases.alias(did.as_key()).map(|a| a.to_string())
}

/// Format a DID for display. The `did:key:` prefix is always stripped.
///
/// When `full` is false: keys are shown as first 7 + `…` + last 7.
/// When `full` is true the complete key is shown. In both cases an alias
/// is prepended as `alice@<key>` when available.
fn format_did(did: &Did, alias: &Option<String>, full: bool) -> String {
    let key = did.to_string().replace("did:key:", "");
    let displayed = if full {
        key
    } else {
        format!("{}…{}", &key[..7], &key[key.len() - 7..])
    };
    match alias {
        Some(alias) => format!("{alias}@{displayed}"),
        None => displayed,
    }
}

/// Append a line to a string buffer.
fn push_line(s: &mut String, line: String) {
    s.push_str(&line);
    s.push('\n');
}

/// Format rows as a column-aligned table, indented by `indent` spaces.
///
/// Computes column widths from all rows in a first pass, then emits each row
/// with cells padded to those widths. Trailing whitespace on each line is
/// trimmed.
fn format_table(rows: &[Vec<String>], indent: usize) -> String {
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; ncols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let prefix = " ".repeat(indent);
    let mut out = String::new();
    for row in rows {
        let mut line = prefix.clone();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 < row.len() {
                line.push_str(&format!("{:<width$}  ", cell, width = widths[i]));
            } else {
                line.push_str(cell);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Resolve the first line of a git commit message for display.
///
/// Implementations typically look up the commit via `git2` and return
/// its summary. Return `None` when the OID cannot be resolved (e.g.
/// it lives in a fork that hasn't been fetched).
pub trait CommitTitle {
    /// Return the first line of the commit message for `oid`, if available.
    fn title(&self, oid: &Oid) -> Option<String>;
}

/// No-op resolver that never produces a title.
impl CommitTitle for () {
    fn title(&self, _oid: &Oid) -> Option<String> {
        None
    }
}

/// Resolve titles from a Radicle git repository.
impl CommitTitle for Repository {
    fn title(&self, oid: &Oid) -> Option<String> {
        self.backend
            .find_commit((*oid).into())
            .ok()
            .and_then(|c| c.summary().map(String::from))
    }
}

/// A set of [`Release`]s sorted by creation time.
#[derive(Serialize)]
pub struct Releases {
    releases: Vec<Release>,
}

impl Releases {
    /// Construct the set of [`Releases`] given an iterator of [`ReleaseId`] and
    /// [`Release`][release] pairs.
    ///
    /// The `aliases` store is used to resolve human-readable aliases for DIDs.
    ///
    /// When `delegates` is provided, artifacts that have been redacted by the
    /// release author, the artifact author, or any delegate are hidden.
    /// Pass `None` to show all artifacts including redacted ones.
    ///
    /// When `show_empty` is false, releases with no visible artifacts are
    /// excluded from the output.
    ///
    /// The `titles` resolver looks up commit summaries for pretty output.
    ///
    /// [release]: crate::Release
    pub fn new(
        releases: impl Iterator<Item = (ReleaseId, crate::Release)>,
        aliases: &impl AliasStore,
        delegates: Option<&BTreeSet<Did>>,
        show_empty: bool,
        titles: &impl CommitTitle,
    ) -> Self {
        let mut releases: Vec<_> = releases
            .map(|(id, release)| {
                let title = titles.title(release.oid());
                Release::new(id, &release, aliases, delegates, title)
            })
            .filter(|r| show_empty || !r.artifacts.is_empty())
            .collect();
        releases.sort_by_key(|r| Reverse(r.created_at));

        Self { releases }
    }

    /// Pretty print the set of [`Release`]s.
    ///
    /// When `verbose` is true, CIDs and NodeIDs are rendered in full rather
    /// than being truncated.
    pub fn pretty(&self, verbose: bool) -> String {
        let mut s = String::new();

        for shown in self.releases.iter() {
            s.push_str(&shown.pretty(verbose));
            s.push('\n');
        }

        s
    }
}

/// A display form of a [`Release`][release].
///
/// [release]: crate::Release
#[derive(Serialize)]
pub struct Release {
    release_id: ReleaseId,
    /// Unix seconds when this release COB was created.
    created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_alias: Option<String>,
    author: Did,
    oid: Oid,
    /// Locally-resolved commit summary; not persisted in the COB.
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    artifacts: Vec<Artifact>,
}

impl Release {
    /// Construct a new [`Release`] display form.
    ///
    /// The `aliases` store is used to resolve human-readable aliases for DIDs.
    ///
    /// When `delegates` is provided, artifacts that have been redacted by the
    /// release author, the artifact author, or any delegate are hidden.
    /// Pass `None` to show all artifacts including redacted ones.
    ///
    /// `title` is the first line of the commit message, if available.
    pub fn new(
        release_id: ReleaseId,
        release: &crate::Release,
        aliases: &impl AliasStore,
        delegates: Option<&BTreeSet<Did>>,
        title: Option<String>,
    ) -> Self {
        let author = *release.author();
        let author_alias = resolve(&author, aliases);
        let mut artifacts: Vec<_> = release
            .artifacts()
            .iter()
            .filter(|(_cid, artifact)| {
                // Hide artifacts redacted by a trusted party: the release
                // author, the artifact author, or a repository delegate.
                if let Some(delegates) = delegates {
                    !artifact.redactions().keys().any(|did| {
                        *did == author || *did == *artifact.author() || delegates.contains(did)
                    })
                } else {
                    true
                }
            })
            .map(|(cid, artifact)| {
                let mut locations: Vec<_> = artifact
                    .locations()
                    .iter()
                    .flat_map(|(did, urls)| {
                        let alias = resolve(did, aliases);
                        urls.iter().map(move |url| Location {
                            alias: alias.clone(),
                            did: *did,
                            url: url.clone(),
                        })
                    })
                    .collect();
                // Sort by (did, url) for deterministic output.
                locations
                    .sort_by(|a, b| a.did.cmp(&b.did).then(a.url.as_str().cmp(b.url.as_str())));
                let attestations: Vec<_> = artifact
                    .attestations()
                    .iter()
                    .map(|did| Attestation {
                        alias: resolve(did, aliases),
                        did: *did,
                    })
                    .collect();
                let mut redactions: Vec<_> = artifact
                    .redactions()
                    .iter()
                    .map(|(did, reason)| Redaction {
                        alias: resolve(did, aliases),
                        did: *did,
                        reason: reason.clone(),
                    })
                    .collect();
                // Sort by DID for deterministic output.
                redactions.sort_by(|a, b| a.did.cmp(&b.did));
                let artifact_author = *artifact.author();
                Artifact {
                    cid: cid.to_string(),
                    author_alias: resolve(&artifact_author, aliases),
                    author: artifact_author,
                    name: artifact.name().to_owned(),
                    locations,
                    attestations,
                    redactions,
                }
            })
            .collect();
        // Sort artifacts by CID string for deterministic output.
        artifacts.sort_by(|a, b| a.cid.cmp(&b.cid));

        Self {
            release_id,
            created_at: release.timestamp(),
            author_alias,
            author,
            oid: *release.oid(),
            title,
            artifacts,
        }
    }

    /// Pretty print a release.
    ///
    /// When `verbose` is true, CIDs and NodeIDs are rendered in full rather
    /// than being truncated.
    pub fn pretty(&self, verbose: bool) -> String {
        let mut s = String::new();

        let author = format_did(&self.author, &self.author_alias, verbose);
        let short_id = &self.release_id.to_string()[..7];
        let short_oid = &self.oid.to_string()[..7];
        let title_suffix = match &self.title {
            Some(t) => format!(" {t}"),
            None => String::new(),
        };
        let date = DateTime::<Utc>::from_timestamp(self.created_at as i64, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| self.created_at.to_string());
        push_line(
            &mut s,
            format!(
                "release {short_id} {date} by {author} (commit {short_oid}{title_suffix})",
            ),
        );
        // Build a per-release artifact table: CID | name | DID | URL.
        // Multiple locations use blank cells in the CID/name columns.
        // Attestations and redactions follow as additional rows.
        let mut rows: Vec<Vec<String>> = Vec::new();
        for artifact in self.artifacts.iter() {
            let cid_cell = if verbose {
                artifact.cid.clone()
            } else {
                // Truncate CID to first 6 and last 6 visible chars for column width.
                format!("{}…{}", &artifact.cid[..6], &artifact.cid[artifact.cid.len() - 6..])
            };

            let mut first = true;
            for loc in artifact.locations.iter() {
                let did = format_did(&loc.did, &loc.alias, verbose);
                if first {
                    rows.push(vec![
                        cid_cell.clone(),
                        artifact.name.clone(),
                        did,
                        loc.url.to_string(),
                    ]);
                    first = false;
                } else {
                    rows.push(vec![
                        String::new(),
                        String::new(),
                        did,
                        loc.url.to_string(),
                    ]);
                }
            }
            if first {
                // No locations — still show CID and name.
                rows.push(vec![cid_cell, artifact.name.clone()]);
            }
            if !artifact.attestations.is_empty() {
                let nodes: Vec<_> = artifact
                    .attestations
                    .iter()
                    .map(|a| format_did(&a.did, &a.alias, verbose))
                    .collect();
                rows.push(vec![
                    String::new(),
                    format!("attestations: {}", nodes.join(", ")),
                ]);
            }
            for r in artifact.redactions.iter() {
                let did = format_did(&r.did, &r.alias, verbose);
                rows.push(vec![
                    String::new(),
                    format!("redacted: {did}"),
                    r.reason.clone(),
                ]);
            }
        }
        s.push_str(&format_table(&rows, 2));

        s
    }
}

#[derive(Serialize)]
struct Artifact {
    cid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_alias: Option<String>,
    author: Did,
    name: String,
    locations: Vec<Location>,
    attestations: Vec<Attestation>,
    redactions: Vec<Redaction>,
}

#[derive(Serialize)]
struct Location {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
    url: Url,
}

#[derive(Serialize)]
struct Attestation {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
}

#[derive(Serialize)]
struct Redaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
    reason: String,
}
