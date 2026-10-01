//! Whether an artifact may be trusted, and why not when it may not.
//!
//! Trust is anchored in the repository's delegate set: by default only
//! releases and artifacts authored by a delegate — or by the local user —
//! count, and a redaction from the artifact's own author or from a
//! delegate withdraws it. The same parties decide the artifact's
//! metadata. [`Trust`] is the single place those rules live, shared by
//! `list`/`show` (through [`crate::Filters`]), by `verify` (does this file
//! match something a delegate published?) and by `watch` (is this artifact
//! worth seeding?).
//!
//! The rules read only the release creator and the artifact, so they can
//! be tested without a repository.

use std::collections::{BTreeMap, BTreeSet};

use radicle::identity::Did;
use serde_json::Value;

use crate::{Artifact, MetadataWrite, Release};

/// Why [`Trust::check`] rejects an artifact in a release.
///
/// Callers can collect these to report the most useful reason. A
/// redaction, for example, is something the user needs to see.
#[derive(Debug, PartialEq, Eq)]
pub enum Untrusted {
    /// Redacted by the artifact's own author or by a delegate.
    Redacted(BTreeMap<Did, String>),
    /// Release created by someone outside the [`Scope`].
    Creator(Did),
    /// Artifact registered by someone outside the [`Scope`].
    Author(Did),
}

/// Whose releases and artifacts the repository trusts: a repository
/// delegate or the local user.
#[derive(Clone, Copy)]
pub struct Trust<'a> {
    /// Delegates of the repository.
    pub delegates: &'a BTreeSet<Did>,
    /// Local user's DID. The local user always trusts their own work,
    /// even when they aren't a delegate.
    pub local: Option<&'a Did>,
}

/// Which side of the [`Trust`] line to accept.
///
/// The scope applies to release creators and artifact authors alike. It
/// never widens who may withdraw an artifact — see [`is_author_or_delegate`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    /// Only delegates and the local user.
    #[default]
    Trusted,
    /// Only users who are neither a delegate nor the local user.
    Untrusted,
    /// Everyone.
    All,
}

impl Scope {
    /// Check whether a party that is `trusted` (or not) falls in this scope.
    pub fn admits(self, trusted: bool) -> bool {
        match self {
            Self::Trusted => trusted,
            Self::Untrusted => !trusted,
            Self::All => true,
        }
    }
}

impl<'a> Trust<'a> {
    /// Trust delegates and the local user.
    pub fn new(delegates: &'a BTreeSet<Did>, local: Option<&'a Did>) -> Self {
        Self { delegates, local }
    }

    /// Check whether `did` is a delegate or the local user.
    pub fn trusts(&self, did: &Did) -> bool {
        self.delegates.contains(did) || self.local == Some(did)
    }

    /// Check whether `did` falls in `scope`.
    pub fn admits(&self, scope: Scope, did: &Did) -> bool {
        scope.admits(self.trusts(did))
    }

    /// Apply the trust rules to `artifact` in `release`: the release creator
    /// and the artifact author must both fall in `scope`, and neither the
    /// author nor a delegate may have redacted it.
    pub fn check(
        &self,
        scope: Scope,
        release: &Release,
        artifact: &Artifact,
    ) -> Result<(), Untrusted> {
        self.check_by(scope, release.creator(), artifact)
    }

    /// [`Self::check`], given only the release's creator.
    fn check_by(&self, scope: Scope, creator: &Did, artifact: &Artifact) -> Result<(), Untrusted> {
        if !self.admits(scope, creator) {
            return Err(Untrusted::Creator(*creator));
        }
        // A redaction from a passing stranger must not block the check, or
        // anyone on the network could veto a release.
        let redactions = withdrawals(artifact, self.delegates);
        if !redactions.is_empty() {
            return Err(Untrusted::Redacted(redactions));
        }
        if !self.admits(scope, &artifact.author) {
            return Err(Untrusted::Author(artifact.author));
        }
        Ok(())
    }
}

/// Only these parties' redactions and metadata writes take effect.
/// A [`Scope`] does not widen this: it decides whose releases and artifacts
/// count, not who may withdraw or annotate one.
pub fn is_author_or_delegate(did: &Did, author: &Did, delegates: &BTreeSet<Did>) -> bool {
    did == author || delegates.contains(did)
}

/// The redactions of `artifact` that withdraw it: those from its author or
/// a delegate. Other redactions carry no authority.
pub fn withdrawals(artifact: &Artifact, delegates: &BTreeSet<Did>) -> BTreeMap<Did, String> {
    artifact
        .redactions
        .iter()
        .filter(|(did, _)| is_author_or_delegate(did, &artifact.author, delegates))
        .map(|(did, reason)| (*did, reason.clone()))
        .collect()
}

/// The metadata of `artifact` that holds: for each key, the last write from
/// its author or a delegate. Other writes carry no authority, or anyone on
/// the network could overwrite a release's metadata.
pub fn metadata(artifact: &Artifact, delegates: &BTreeSet<Did>) -> BTreeMap<String, Value> {
    artifact
        .metadata
        .iter()
        .filter_map(|(key, writes)| {
            let (_, write) = writes
                .iter()
                .rev()
                .find(|(did, _)| is_author_or_delegate(did, &artifact.author, delegates))?;
            match write {
                MetadataWrite::Set(value) => Some((key.clone(), value.clone())),
                MetadataWrite::Removed => None,
            }
        })
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
    fn check(creator: u8, artifact: &Artifact, scope: Scope) -> Result<(), Untrusted> {
        let delegates = delegates();
        let local = did(LOCAL);
        Trust::new(&delegates, Some(&local)).check_by(scope, &did(creator), artifact)
    }

    #[test]
    fn delegate_registered_artifact_verifies() {
        assert_eq!(
            check(DELEGATE, &artifact(DELEGATE, &[]), Scope::Trusted),
            Ok(())
        );
    }

    #[test]
    fn stranger_authored_artifact_is_rejected() {
        let artifact = artifact(STRANGER, &[]);
        assert_eq!(
            check(DELEGATE, &artifact, Scope::Trusted),
            Err(Untrusted::Author(did(STRANGER)))
        );
        // `--all-authors` is exactly the opt-in for this case.
        assert_eq!(check(DELEGATE, &artifact, Scope::All), Ok(()));
    }

    #[test]
    fn stranger_created_release_is_rejected() {
        let artifact = artifact(DELEGATE, &[]);
        assert_eq!(
            check(STRANGER, &artifact, Scope::Trusted),
            Err(Untrusted::Creator(did(STRANGER)))
        );
        assert_eq!(check(STRANGER, &artifact, Scope::All), Ok(()));
    }

    #[test]
    fn untrusted_scope_applies_to_creator_and_author() {
        let theirs = artifact(STRANGER, &[]);
        assert_eq!(check(STRANGER, &theirs, Scope::Untrusted), Ok(()));
        // Strict: a stranger's artifact in a delegate's release is out.
        assert_eq!(
            check(DELEGATE, &theirs, Scope::Untrusted),
            Err(Untrusted::Creator(did(DELEGATE)))
        );
        assert_eq!(
            check(STRANGER, &artifact(LOCAL, &[]), Scope::Untrusted),
            Err(Untrusted::Author(did(LOCAL)))
        );
    }

    #[test]
    fn own_artifact_verifies_without_all_authors() {
        // The local user is not a delegate here, but must still be able to
        // verify what they registered themselves.
        assert_eq!(check(LOCAL, &artifact(LOCAL, &[]), Scope::Trusted), Ok(()));
    }

    #[test]
    fn delegate_redaction_is_rejected() {
        let artifact = artifact(DELEGATE, &[(OTHER_DELEGATE, "compromised")]);
        let err = check(DELEGATE, &artifact, Scope::Trusted).unwrap_err();
        assert_eq!(
            err,
            Untrusted::Redacted(BTreeMap::from([(
                did(OTHER_DELEGATE),
                "compromised".to_owned()
            )]))
        );
        // A redaction is a withdrawal by a trusted party; --all-authors
        // widens who may register an artifact, not who may withdraw one.
        assert!(check(DELEGATE, &artifact, Scope::All).is_err());
    }

    #[test]
    fn self_redaction_is_rejected() {
        let artifact = artifact(DELEGATE, &[(DELEGATE, "bad build")]);
        assert!(matches!(
            check(DELEGATE, &artifact, Scope::Trusted),
            Err(Untrusted::Redacted(_))
        ));
    }

    #[test]
    fn stranger_redaction_does_not_block() {
        // Redacting is open to anyone on the network, so honouring an
        // untrusted redaction would let any peer veto a release.
        let artifact = artifact(DELEGATE, &[(STRANGER, "trust me")]);
        assert_eq!(check(DELEGATE, &artifact, Scope::Trusted), Ok(()));
    }

    #[test]
    fn only_trusted_redactions_are_reported() {
        let artifact = artifact(
            DELEGATE,
            &[(STRANGER, "noise"), (OTHER_DELEGATE, "real reason")],
        );
        let Err(Untrusted::Redacted(reported)) = check(DELEGATE, &artifact, Scope::Trusted) else {
            panic!("expected a redaction");
        };
        assert_eq!(
            reported,
            BTreeMap::from([(did(OTHER_DELEGATE), "real reason".to_owned())])
        );
    }
}
