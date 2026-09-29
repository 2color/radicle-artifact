//! Whether an artifact may be trusted, and why not when it may not.
//!
//! Trust is anchored in the repository's delegate set: by default only
//! releases and artifacts authored by a delegate — or by the local user —
//! count, and a redaction from the artifact's own author or from a
//! delegate withdraws it. [`Trust`] is the single place those rules
//! live, shared by `list`/`show` (through [`crate::Filters`]), by `verify`
//! (does this file match something a delegate published?) and by `watch`
//! (is this artifact worth seeding?).
//!
//! The rules read only the release creator and the artifact, so they can
//! be tested without a repository.

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
    /// Release created by someone who is neither a delegate nor the local user.
    Creator(Did),
    /// Artifact registered by someone who is neither a delegate nor the local user.
    Author(Did),
}

/// Whose releases and artifacts the repository trusts.
///
/// By default that is a repository delegate or the local user.
/// `all_authors` opens it up to everyone who registers or creates, but
/// never to who may withdraw — see [`may_amend`].
#[derive(Clone, Copy)]
pub struct Trust<'a> {
    /// Delegates of the repository.
    pub delegates: &'a BTreeSet<Did>,
    /// Local user's DID. The local user always trusts their own work,
    /// even when they aren't a delegate.
    pub local: Option<&'a Did>,
    /// When true, trust any author, not only delegates and the local user.
    pub all_authors: bool,
}

impl Trust<'_> {
    /// Check whether `did` may create a release or register an artifact:
    /// a delegate, the local user, or anyone when `all_authors` is set.
    pub fn trusts(&self, did: &Did) -> bool {
        self.all_authors || self.delegates.contains(did) || self.local == Some(did)
    }

    /// Apply the trust rules to `artifact` in `release`: the release and the
    /// artifact must both be authored by a trusted party, and no party
    /// that [`may_amend`] the artifact may have redacted it.
    pub fn classify(&self, release: &Release, artifact: &Artifact) -> Result<(), Untrusted> {
        self.classify_by(release.creator(), artifact)
    }

    /// [`Self::classify`], given only the release's creator.
    fn classify_by(&self, creator: &Did, artifact: &Artifact) -> Result<(), Untrusted> {
        if !self.trusts(creator) {
            return Err(Untrusted::Creator(*creator));
        }
        // A redaction from a passing stranger must not block the check, or
        // anyone on the network could veto a release.
        let redactions = withdrawals(artifact, self.delegates);
        if !redactions.is_empty() {
            return Err(Untrusted::Redacted(redactions));
        }
        if !self.trusts(&artifact.author) {
            return Err(Untrusted::Author(artifact.author));
        }
        Ok(())
    }
}

/// Check whether `did` may amend an artifact by `author` — withdraw it or
/// change its metadata: only the author and the delegates may. `all_authors`
/// does not widen this — it opens up who may register an artifact, not who
/// may amend one.
pub fn may_amend(did: &Did, author: &Did, delegates: &BTreeSet<Did>) -> bool {
    did == author || delegates.contains(did)
}

/// The redactions of `artifact` that withdraw it: those from a party that
/// [`may_amend`] it. Other redactions carry no authority.
pub fn withdrawals(artifact: &Artifact, delegates: &BTreeSet<Did>) -> BTreeMap<Did, String> {
    artifact
        .redactions
        .iter()
        .filter(|(did, _)| may_amend(did, &artifact.author, delegates))
        .map(|(did, reason)| (*did, reason.clone()))
        .collect()
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

    /// An artifact registered by `author`, redacted by each of `redactions`.
    fn artifact(author: u8, redactions: &[(u8, &str)]) -> Artifact {
        Artifact {
            author: did(author),
            name: "bin".to_owned(),
            locations: BTreeMap::new(),
            attestations: BTreeSet::new(),
            redactions: redactions
                .iter()
                .map(|(n, reason)| (did(*n), (*reason).to_owned()))
                .collect(),
            metadata: BTreeMap::new(),
        }
    }

    /// Classify `artifact` in a release created by `creator`.
    fn check(creator: u8, artifact: &Artifact, all_authors: bool) -> Result<(), Untrusted> {
        let delegates = delegates();
        let local = did(LOCAL);
        Trust {
            delegates: &delegates,
            local: Some(&local),
            all_authors,
        }
        .classify_by(&did(creator), artifact)
    }

    #[test]
    fn delegate_registered_artifact_verifies() {
        assert_eq!(check(DELEGATE, &artifact(DELEGATE, &[]), false), Ok(()));
    }

    #[test]
    fn stranger_authored_artifact_is_rejected() {
        let artifact = artifact(STRANGER, &[]);
        assert_eq!(
            check(DELEGATE, &artifact, false),
            Err(Untrusted::Author(did(STRANGER)))
        );
        // `--all-authors` is exactly the opt-in for this case.
        assert_eq!(check(DELEGATE, &artifact, true), Ok(()));
    }

    #[test]
    fn stranger_created_release_is_rejected() {
        let artifact = artifact(DELEGATE, &[]);
        assert_eq!(
            check(STRANGER, &artifact, false),
            Err(Untrusted::Creator(did(STRANGER)))
        );
        assert_eq!(check(STRANGER, &artifact, true), Ok(()));
    }

    #[test]
    fn own_artifact_verifies_without_all_authors() {
        // The local user is not a delegate here, but must still be able to
        // verify what they registered themselves.
        assert_eq!(check(LOCAL, &artifact(LOCAL, &[]), false), Ok(()));
    }

    #[test]
    fn delegate_redaction_is_rejected() {
        let artifact = artifact(DELEGATE, &[(OTHER_DELEGATE, "compromised")]);
        let err = check(DELEGATE, &artifact, false).unwrap_err();
        assert_eq!(
            err,
            Untrusted::Redacted(BTreeMap::from([(
                did(OTHER_DELEGATE),
                "compromised".to_owned()
            )]))
        );
        // A redaction is a withdrawal by a trusted party; --all-authors
        // widens who may register an artifact, not who may withdraw one.
        assert!(check(DELEGATE, &artifact, true).is_err());
    }

    #[test]
    fn self_redaction_is_rejected() {
        let artifact = artifact(DELEGATE, &[(DELEGATE, "bad build")]);
        assert!(matches!(
            check(DELEGATE, &artifact, false),
            Err(Untrusted::Redacted(_))
        ));
    }

    #[test]
    fn stranger_redaction_does_not_block() {
        // Redacting is open to anyone on the network, so honouring an
        // untrusted redaction would let any peer veto a release.
        let artifact = artifact(DELEGATE, &[(STRANGER, "trust me")]);
        assert_eq!(check(DELEGATE, &artifact, false), Ok(()));
    }

    #[test]
    fn only_trusted_redactions_are_reported() {
        let artifact = artifact(
            DELEGATE,
            &[(STRANGER, "noise"), (OTHER_DELEGATE, "real reason")],
        );
        let Err(Untrusted::Redacted(reported)) = check(DELEGATE, &artifact, false) else {
            panic!("expected a redaction");
        };
        assert_eq!(
            reported,
            BTreeMap::from([(did(OTHER_DELEGATE), "real reason".to_owned())])
        );
    }
}
