//! Whether an artifact may be trusted, and why not when it may not.
//!
//! Trust is anchored in the repository's delegate set: by default only
//! releases and artifacts authored by a delegate — or by the local user —
//! count, and a redaction from the artifact's own author or from a
//! delegate withdraws it. [`classify`] is the single place those rules
//! live, shared by `verify` (does this file match something a delegate
//! published?) and by `watch` (is this artifact worth seeding?).
//!
//! The rules read from a [`Candidate`] rather than from a repository, so
//! they can be tested without one.

use std::collections::{BTreeMap, BTreeSet};

use radicle::identity::Did;

use crate::{Artifact, Release};

/// Why a release that registers a CID doesn't count as verification.
///
/// Kept so a failed check can report the most useful reason instead of a
/// flat "not found" — a redaction in particular is something the user
/// needs to see.
#[derive(Debug, PartialEq, Eq)]
pub enum Untrusted {
    /// Redacted by the artifact's own author or by a delegate.
    Redacted(BTreeMap<Did, String>),
    /// Registered by someone who is neither a delegate nor the local user.
    Author(Did),
}

/// The trust inputs read from one release that registers the CID being
/// checked. Extracted from the COB so the rules below can be tested
/// without a repository.
pub struct Candidate {
    /// DID that created the release COB.
    pub release_creator: Did,
    /// DID that registered the artifact within that release.
    pub artifact_author: Did,
    /// Every DID that has redacted the artifact, with its stated reason.
    pub redactions: BTreeMap<Did, String>,
}

impl Candidate {
    /// Read the trust inputs out of a release and one of its artifacts.
    pub fn new(release: &Release, artifact: &Artifact) -> Self {
        Self {
            release_creator: *release.creator(),
            artifact_author: *artifact.author(),
            redactions: artifact.redactions().clone(),
        }
    }
}

/// Apply the trust rules `list`/`show` already use (see
/// [`crate::display::Filters`]) to a single candidate: the release and the
/// artifact must both be authored by a delegate or by us, and no trusted
/// party may have redacted the artifact.
pub fn classify(
    candidate: &Candidate,
    delegates: &BTreeSet<Did>,
    local: &Did,
    all_authors: bool,
) -> Result<(), Untrusted> {
    let trusted = |did: &Did| all_authors || delegates.contains(did) || did == local;

    if !trusted(&candidate.release_creator) {
        return Err(Untrusted::Author(candidate.release_creator));
    }
    // A redaction counts when it comes from the artifact's own author or
    // from a delegate; one from a passing stranger must not block the
    // check, or anyone on the network could veto a release. `--all-authors`
    // deliberately does not widen this — it opens up who may *register* an
    // artifact, not who may withdraw one.
    let redactions: BTreeMap<Did, String> = candidate
        .redactions
        .iter()
        .filter(|(did, _)| **did == candidate.artifact_author || delegates.contains(did))
        .map(|(did, reason)| (*did, reason.clone()))
        .collect();
    if !redactions.is_empty() {
        return Err(Untrusted::Redacted(redactions));
    }
    if !trusted(&candidate.artifact_author) {
        return Err(Untrusted::Author(candidate.artifact_author));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use radicle::crypto::PublicKey;

    use super::*;

    /// Distinct DIDs keyed by a single byte, so a test can name its
    /// participants. Never used to verify a signature, so the bytes don't
    /// have to be a valid curve point.
    fn did(n: u8) -> Did {
        Did::from(PublicKey::from_bytes([n; 32]))
    }

    const DELEGATE: u8 = 1;
    const OTHER_DELEGATE: u8 = 2;
    const LOCAL: u8 = 3;
    const STRANGER: u8 = 4;

    fn delegates() -> BTreeSet<Did> {
        BTreeSet::from([did(DELEGATE), did(OTHER_DELEGATE)])
    }

    /// A candidate a delegate created and registered, with no redactions.
    fn trusted_candidate() -> Candidate {
        Candidate {
            release_creator: did(DELEGATE),
            artifact_author: did(DELEGATE),
            redactions: BTreeMap::new(),
        }
    }

    fn check(candidate: &Candidate, all_authors: bool) -> Result<(), Untrusted> {
        classify(candidate, &delegates(), &did(LOCAL), all_authors)
    }

    #[test]
    fn delegate_registered_artifact_verifies() {
        assert_eq!(check(&trusted_candidate(), false), Ok(()));
    }

    #[test]
    fn stranger_authored_artifact_is_rejected() {
        let candidate = Candidate {
            artifact_author: did(STRANGER),
            ..trusted_candidate()
        };
        assert_eq!(
            check(&candidate, false),
            Err(Untrusted::Author(did(STRANGER)))
        );
        // `--all-authors` is exactly the opt-in for this case.
        assert_eq!(check(&candidate, true), Ok(()));
    }

    #[test]
    fn stranger_created_release_is_rejected() {
        let candidate = Candidate {
            release_creator: did(STRANGER),
            ..trusted_candidate()
        };
        assert_eq!(
            check(&candidate, false),
            Err(Untrusted::Author(did(STRANGER)))
        );
        assert_eq!(check(&candidate, true), Ok(()));
    }

    #[test]
    fn own_artifact_verifies_without_all_authors() {
        // The local user is not a delegate here, but must still be able to
        // verify what they registered themselves.
        let candidate = Candidate {
            release_creator: did(LOCAL),
            artifact_author: did(LOCAL),
            redactions: BTreeMap::new(),
        };
        assert_eq!(check(&candidate, false), Ok(()));
    }

    #[test]
    fn delegate_redaction_is_rejected() {
        let candidate = Candidate {
            redactions: BTreeMap::from([(did(OTHER_DELEGATE), "compromised".to_owned())]),
            ..trusted_candidate()
        };
        let err = check(&candidate, false).unwrap_err();
        assert_eq!(
            err,
            Untrusted::Redacted(BTreeMap::from([(
                did(OTHER_DELEGATE),
                "compromised".to_owned()
            )]))
        );
        // A redaction is a withdrawal by a trusted party; --all-authors
        // widens who may register an artifact, not who may withdraw one.
        assert!(check(&candidate, true).is_err());
    }

    #[test]
    fn self_redaction_is_rejected() {
        let candidate = Candidate {
            redactions: BTreeMap::from([(did(DELEGATE), "bad build".to_owned())]),
            ..trusted_candidate()
        };
        assert!(matches!(
            check(&candidate, false),
            Err(Untrusted::Redacted(_))
        ));
    }

    #[test]
    fn stranger_redaction_does_not_block() {
        // Redacting is open to anyone on the network, so honouring an
        // untrusted redaction would let any peer veto a release.
        let candidate = Candidate {
            redactions: BTreeMap::from([(did(STRANGER), "trust me".to_owned())]),
            ..trusted_candidate()
        };
        assert_eq!(check(&candidate, false), Ok(()));
    }

    #[test]
    fn only_trusted_redactions_are_reported() {
        let candidate = Candidate {
            redactions: BTreeMap::from([
                (did(STRANGER), "noise".to_owned()),
                (did(OTHER_DELEGATE), "real reason".to_owned()),
            ]),
            ..trusted_candidate()
        };
        let Err(Untrusted::Redacted(reported)) = check(&candidate, false) else {
            panic!("expected a redaction");
        };
        assert_eq!(
            reported,
            BTreeMap::from([(did(OTHER_DELEGATE), "real reason".to_owned())])
        );
    }
}
