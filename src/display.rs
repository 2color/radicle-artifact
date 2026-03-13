//! Display forms of Release data.
//!
//! These can be used in tools that wish to display data, such as the
//! `rad-artifact` CLI tool.

use radicle::{git::Oid, node::NodeId};
use serde::Serialize;
use url::Url;

use crate::{Cid, ReleaseId};

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
                    .map(|(node_id, urls)| {
                        let mut urls = urls.clone();
                        urls.sort();
                        NodeLocations {
                            node_id: *node_id,
                            urls,
                        }
                    })
                    .collect();
                locations.sort_by_cached_key(|l| l.node_id);
                Artifact {
                    cid: *cid,
                    name: artifact.name().to_owned(),
                    locations,
                }
            })
            .collect();
        // Sort artifacts by CID for deterministic output.
        artifacts.sort_by(|a, b| a.cid.cmp(&b.cid));

        Self {
            release_id,
            oid: *release.oid(),
            artifacts,
        }
    }

    /// Pretty print a release.
    pub fn pretty(&self) -> String {
        let mut s = String::new();

        push_line(
            &mut s,
            format!("release {} (commit {})", self.release_id, self.oid),
        );
        for artifact in self.artifacts.iter() {
            push_line(
                &mut s,
                format!("  artifact {} ({})", artifact.cid, artifact.name),
            );
            for node_locs in artifact.locations.iter() {
                push_line(&mut s, format!("    node {}", node_locs.node_id));
                for url in node_locs.urls.iter() {
                    push_line(&mut s, format!("      {url}"));
                }
            }
        }

        s
    }
}

#[derive(Serialize)]
struct Artifact {
    cid: Cid,
    name: String,
    locations: Vec<NodeLocations>,
}

#[derive(Serialize)]
struct NodeLocations {
    node_id: NodeId,
    urls: Vec<Url>,
}