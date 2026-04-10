//! Display forms of Release data.
//!
//! These can be used in tools that wish to display data, such as the
//! `rad-artifact` CLI tool.

use radicle::{
    git::Oid,
    identity::Did,
    node::AliasStore,
};
use serde::Serialize;
use url::Url;

use crate::ReleaseId;

/// Resolve a DID's alias via an [`AliasStore`], returning the string if found.
fn resolve(did: &Did, aliases: &impl AliasStore) -> Option<String> {
    aliases.alias(did.as_key()).map(|a| a.to_string())
}

/// Format a DID, prefixed with its alias if available.
fn format_did(did: &Did, alias: &Option<String>) -> String {
    match alias {
        Some(alias) => format!("{alias} ({did})"),
        None => did.to_string(),
    }
}

/// Append a line to a string buffer.
fn push_line(s: &mut String, line: String) {
    s.push_str(&line);
    s.push('\n');
}

/// A set of [`Release`]s sorted by their [`ReleaseId`].
#[derive(Serialize)]
pub struct Releases {
    count: usize,
    releases: Vec<Release>,
}

impl Releases {
    /// Construct the set of [`Releases`] given an iterator of [`ReleaseId`] and
    /// [`Release`][release] pairs.
    ///
    /// The `aliases` store is used to resolve human-readable aliases for DIDs.
    ///
    /// [release]: crate::Release
    pub fn new(
        releases: impl Iterator<Item = (ReleaseId, crate::Release)>,
        aliases: &impl AliasStore,
    ) -> Self {
        let mut releases = releases
            .map(|(id, release)| Release::new(id, &release, aliases))
            .collect::<Vec<_>>();
        releases.sort_by_cached_key(|r| r.release_id);

        Self {
            count: releases.len(),
            releases,
        }
    }

    /// Pretty print the set of [`Releases`] and their count.
    pub fn pretty(&self) -> String {
        let mut s = String::new();

        push_line(&mut s, format!("count: {}", self.count));
        for shown in self.releases.iter() {
            s.push_str(&shown.pretty());
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
    #[serde(skip_serializing_if = "Option::is_none")]
    author_alias: Option<String>,
    author: Did,
    oid: Oid,
    artifacts: Vec<Artifact>,
}

impl Release {
    /// Construct a new [`Release`] display form.
    ///
    /// The `aliases` store is used to resolve human-readable aliases for DIDs.
    pub fn new(
        release_id: ReleaseId,
        release: &crate::Release,
        aliases: &impl AliasStore,
    ) -> Self {
        let author = *release.author();
        let author_alias = resolve(&author, aliases);
        let mut artifacts: Vec<_> = release
            .artifacts()
            .iter()
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
                locations.sort_by(|a, b| a.did.cmp(&b.did).then(a.url.as_str().cmp(b.url.as_str())));
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
            author_alias,
            author,
            oid: *release.oid(),
            artifacts,
        }
    }

    /// Pretty print a release.
    pub fn pretty(&self) -> String {
        let mut s = String::new();

        let author = format_did(&self.author, &self.author_alias);
        push_line(
            &mut s,
            format!("release {} by {} (commit {})", self.release_id, author, self.oid),
        );
        for artifact in self.artifacts.iter() {
            push_line(
                &mut s,
                format!("  artifact {} ({})", artifact.cid, artifact.name),
            );
            for loc in artifact.locations.iter() {
                let did = format_did(&loc.did, &loc.alias);
                push_line(&mut s, format!("    {did} {}", loc.url));
            }
            if !artifact.attestations.is_empty() {
                let nodes: Vec<_> = artifact
                    .attestations
                    .iter()
                    .map(|a| format_did(&a.did, &a.alias))
                    .collect();
                push_line(
                    &mut s,
                    format!("    attestations: {}", nodes.join(", ")),
                );
            }
            if !artifact.redactions.is_empty() {
                push_line(&mut s, "    redactions:".to_string());
                for r in artifact.redactions.iter() {
                    let did = format_did(&r.did, &r.alias);
                    push_line(&mut s, format!("      {did} - {}", r.reason));
                }
            }
        }

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