//! Display forms of Release data.
//!
//! These can be used in tools that wish to display data, such as the
//! `rad-artifact` CLI tool.

use radicle::{git::Oid, identity::Did};
use serde::Serialize;
use url::Url;

use crate::ReleaseId;

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
    /// [release]: crate::Release
    pub fn new(releases: impl Iterator<Item = (ReleaseId, crate::Release)>) -> Self {
        let mut releases = releases
            .map(|(id, release)| Release::new(id, &release))
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
    author: Did,
    oid: Oid,
    artifacts: Vec<Artifact>,
}

impl Release {
    /// Construct a new [`Release`] display form.
    pub fn new(release_id: ReleaseId, release: &crate::Release) -> Self {
        let mut artifacts: Vec<_> = release
            .artifacts()
            .iter()
            .map(|(cid, artifact)| {
                let mut locations: Vec<_> = artifact
                    .locations()
                    .iter()
                    .flat_map(|(did, urls)| {
                        urls.iter().map(move |url| Location {
                            did: *did,
                            url: url.clone(),
                        })
                    })
                    .collect();
                // Sort by (did, url) for deterministic output.
                locations.sort_by(|a, b| a.did.cmp(&b.did).then(a.url.as_str().cmp(b.url.as_str())));
                let attestations: Vec<_> = artifact.attestations().iter().copied().collect();
                let mut redactions: Vec<_> = artifact
                    .redactions()
                    .iter()
                    .map(|(did, reason)| Redaction {
                        did: *did,
                        reason: reason.clone(),
                    })
                    .collect();
                // Sort by DID for deterministic output.
                redactions.sort_by(|a, b| a.did.cmp(&b.did));
                Artifact {
                    cid: cid.to_string(),
                    author: *artifact.author(),
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
            author: *release.author(),
            oid: *release.oid(),
            artifacts,
        }
    }

    /// Pretty print a release.
    pub fn pretty(&self) -> String {
        let mut s = String::new();

        push_line(
            &mut s,
            format!("release {} by {} (commit {})", self.release_id, self.author, self.oid),
        );
        for artifact in self.artifacts.iter() {
            push_line(
                &mut s,
                format!("  artifact {} ({})", artifact.cid, artifact.name),
            );
            for loc in artifact.locations.iter() {
                push_line(
                    &mut s,
                    format!("    {} {}", loc.did, loc.url),
                );
            }
            if !artifact.attestations.is_empty() {
                let nodes: Vec<_> = artifact.attestations.iter().map(|n| n.to_string()).collect();
                push_line(
                    &mut s,
                    format!("    attestations: {}", nodes.join(", ")),
                );
            }
            if !artifact.redactions.is_empty() {
                push_line(&mut s, "    redactions:".to_string());
                for r in artifact.redactions.iter() {
                    push_line(&mut s, format!("      {} - {}", r.did, r.reason));
                }
            }
        }

        s
    }
}

#[derive(Serialize)]
struct Artifact {
    cid: String,
    author: Did,
    name: String,
    locations: Vec<Location>,
    attestations: Vec<Did>,
    redactions: Vec<Redaction>,
}

#[derive(Serialize)]
struct Location {
    did: Did,
    url: Url,
}

#[derive(Serialize)]
struct Redaction {
    did: Did,
    reason: String,
}
