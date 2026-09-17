//! What a watching node should seed, and whether it has room for it.
//!
//! The `rad-artifact watch` command keeps a node's seeded set in step with
//! the artifacts its trusted peers publish. This module holds the two
//! decisions it makes — *is this artifact worth seeding?* and *is there
//! room?* — with no I/O, so both are testable without a repository or a
//! running node.
//!
//! The driving loop (subscribe, fetch, add the location) lives in the CLI,
//! because adding a `radiroh://` Location is a signed COB write and the
//! seeding node writes no COBs.

use std::collections::{BTreeSet, HashSet};

use radicle::identity::Did;

use crate::trust::{classify, Candidate};
use crate::{Cid, Release, ReleaseId, METADATA_KEY_SIZE_BYTES};

/// One artifact a watching node would seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// Release to tag the seed under, and to add our Location to.
    pub release_id: ReleaseId,
    /// Content identifier to fetch.
    pub cid: Cid,
    /// Artifact name, for logging.
    pub name: String,
    /// The `sizeBytes` hint, when the registering author recorded one.
    pub size_hint: Option<u64>,
}

/// The trusted artifacts in a repository that a node is not seeding yet,
/// newest release first.
///
/// Trust is [`classify`] with `all_authors: false` — the same rule
/// `verify` applies — so a release or artifact from a stranger is skipped,
/// and a redaction by the artifact's author or by a delegate withdraws it.
///
/// `is_seeded` is asked per CID, matching the node's own `IsSeeding`, which
/// is repository-scoped rather than release-scoped. For the same reason a
/// CID registered in several releases is wanted only once, under the newest
/// release that carries it.
pub fn wanted(
    releases: &[(ReleaseId, Release)],
    delegates: &BTreeSet<Did>,
    local: &Did,
    is_seeded: impl Fn(&Cid) -> bool,
) -> Vec<Wanted> {
    // Ordered by reference: the caller keeps the releases to read Locations
    // from, so there is nothing to gain from taking a copy of them.
    let mut ordered: Vec<&(ReleaseId, Release)> = releases.iter().collect();
    ordered.sort_by_key(|(_, r)| std::cmp::Reverse(r.timestamp()));

    let mut seen: HashSet<Cid> = HashSet::new();
    let mut wanted = Vec::new();
    for (release_id, release) in ordered {
        for (cid, artifact) in release.artifacts() {
            if !seen.insert(*cid) {
                continue;
            }
            if classify(&Candidate::new(release, artifact), delegates, local, false).is_err() {
                continue;
            }
            if is_seeded(cid) {
                continue;
            }
            wanted.push(Wanted {
                release_id: *release_id,
                cid: *cid,
                name: artifact.name().to_owned(),
                size_hint: size_hint(artifact),
            });
        }
    }
    wanted
}

/// The `sizeBytes` metadata hint, when present and a plain integer.
fn size_hint(artifact: &crate::Artifact) -> Option<u64> {
    artifact.metadata().get(METADATA_KEY_SIZE_BYTES)?.as_u64()
}

/// Whether seeding an artifact keeps the store within `limit` total bytes.
///
/// `limit` of `None` is unbounded. An artifact with no size hint is
/// admitted whenever the store is already under the limit — the hint is
/// optional metadata, and refusing everything that lacks one would make
/// the budget a stricter filter than it claims to be. The caller re-reads
/// `seeded_bytes` before each fetch, so the real sizes catch up.
pub fn fits(limit: Option<u64>, seeded_bytes: u64, size_hint: Option<u64>) -> bool {
    let Some(limit) = limit else {
        return true;
    };
    match size_hint {
        Some(size) => seeded_bytes.saturating_add(size) <= limit,
        None => seeded_bytes < limit,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::str::FromStr;

    use radicle::crypto::PublicKey;
    use radicle_artifact_core::cid::{blake3_hash_to_cid, ArtifactKind};

    use super::*;

    fn did(n: u8) -> Did {
        Did::from(PublicKey::from_bytes([n; 32]))
    }

    fn cid(seed: &[u8]) -> Cid {
        blake3_hash_to_cid(blake3::hash(seed), ArtifactKind::Blob)
    }

    const DELEGATE: u8 = 1;
    const LOCAL: u8 = 3;
    const STRANGER: u8 = 4;

    fn delegates() -> BTreeSet<Did> {
        BTreeSet::from([did(DELEGATE)])
    }

    /// Build a release carrying one artifact through serde, so the fixture
    /// needs no repository and no test-only constructors on the COB types.
    /// `n` seeds both the release id and its timestamp, so a higher `n` is
    /// the newer release.
    fn release(
        n: u8,
        creator: u8,
        author: u8,
        cid: Cid,
        redactions: &[(u8, &str)],
        size: Option<u64>,
    ) -> (ReleaseId, Release) {
        let redactions: serde_json::Map<_, _> = redactions
            .iter()
            .map(|(did_n, reason)| (did(*did_n).to_string(), serde_json::json!(reason)))
            .collect();
        let metadata = match size {
            Some(size) => serde_json::json!({ METADATA_KEY_SIZE_BYTES: size }),
            None => serde_json::json!({}),
        };
        let release: Release = serde_json::from_value(serde_json::json!({
            "oid": hex(n),
            "creator": did(creator).to_string(),
            "timestamp": n as u64,
            "artifacts": {
                cid.to_string(): {
                    "author": did(author).to_string(),
                    "name": "artifact",
                    "locations": {},
                    "redactions": redactions,
                    "metadata": metadata,
                }
            },
        }))
        .unwrap();
        (ReleaseId::from_str(&hex(n)).unwrap(), release)
    }

    /// A distinct, well-formed object id per fixture.
    fn hex(n: u8) -> String {
        format!("{:0>40}", format!("{n:x}"))
    }

    fn seeds_nothing(_: &Cid) -> bool {
        false
    }

    #[test]
    fn delegate_artifact_is_wanted() {
        let r = release(1, DELEGATE, DELEGATE, cid(b"a"), &[], Some(10));
        let got = wanted(&[r], &delegates(), &did(LOCAL), seeds_nothing);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].cid, cid(b"a"));
        assert_eq!(got[0].size_hint, Some(10));
    }

    #[test]
    fn stranger_artifact_is_not_wanted() {
        let r = release(1, STRANGER, STRANGER, cid(b"a"), &[], None);
        assert!(wanted(&[r], &delegates(), &did(LOCAL), seeds_nothing).is_empty());
    }

    #[test]
    fn delegate_redaction_withdraws_the_artifact() {
        let r = release(
            1,
            DELEGATE,
            DELEGATE,
            cid(b"a"),
            &[(DELEGATE, "compromised")],
            None,
        );
        assert!(wanted(&[r], &delegates(), &did(LOCAL), seeds_nothing).is_empty());
    }

    #[test]
    fn already_seeded_is_skipped() {
        let r = release(1, DELEGATE, DELEGATE, cid(b"a"), &[], None);
        assert!(wanted(&[r], &delegates(), &did(LOCAL), |_| true).is_empty());
    }

    #[test]
    fn a_cid_in_two_releases_is_wanted_once_under_the_newest() {
        let old = release(1, DELEGATE, DELEGATE, cid(b"a"), &[], None);
        let new = release(2, DELEGATE, DELEGATE, cid(b"a"), &[], None);
        let new_id = new.0;
        let got = wanted(&[old, new], &delegates(), &did(LOCAL), seeds_nothing);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].release_id, new_id);
    }

    #[test]
    fn no_limit_always_fits() {
        assert!(fits(None, u64::MAX, Some(u64::MAX)));
    }

    #[test]
    fn a_hint_over_the_limit_does_not_fit() {
        assert!(fits(Some(100), 40, Some(60)));
        assert!(!fits(Some(100), 41, Some(60)));
    }

    #[test]
    fn a_missing_hint_fits_while_under_the_limit() {
        assert!(fits(Some(100), 99, None));
        assert!(!fits(Some(100), 100, None));
    }
}
