//! Visibility rules for releases and artifacts.

use crate::trust::{Scope, Trust};
use crate::{Artifact, Cid, Release};

/// Visibility rules for releases and artifacts, as `list` / `show` apply them.
///
/// [`Trust`] draws the line between trusted and untrusted parties, and
/// [`Scope`] picks which side of it to show. `include_redacted` opts into
/// withdrawn artifacts.
#[derive(Clone, Copy)]
pub struct Filters<'a> {
    /// Who is trusted.
    pub trust: Trust<'a>,
    /// Whose releases and artifacts are shown: trusted, untrusted or all.
    pub scope: Scope,
    /// When true, include artifacts redacted by their author or by a delegate.
    pub include_redacted: bool,
}

impl<'a> Filters<'a> {
    /// Check whether `list` / `show` include `artifact`:
    /// - Artifacts redacted by a trusted party are hidden, unless
    ///   `include_redacted` is set.
    /// - Artifacts whose author is outside `scope` are hidden.
    pub fn shows_artifact(&self, artifact: &Artifact) -> bool {
        if !self.include_redacted && artifact.is_redacted_by_trusted(self.trust.delegates) {
            return false;
        }
        // Delegates are the curated source of truth for a repo; non-delegate
        // contributions are opt-in. The local user always sees their own.
        self.trust.admits(self.scope, artifact.author())
    }

    /// The artifacts in `release` that [`Self::shows_artifact`] lets through.
    pub fn artifacts(self, release: &'a Release) -> impl Iterator<Item = (&'a Cid, &'a Artifact)> {
        release
            .artifacts()
            .iter()
            .filter(move |(_, artifact)| self.shows_artifact(artifact))
    }

    /// Check whether `list` shows `release`: the creator must be in `scope`,
    /// and a trusted party must not have redacted every artifact, unless
    /// `include_redacted` is set. Artifact authors do not matter here; they only
    /// decide which artifacts [`Self::shows_artifact`] lists.
    pub fn shows_release(&self, release: &Release) -> bool {
        self.trust.admits(self.scope, release.creator())
            && (self.include_redacted || !release.is_fully_redacted(self.trust.delegates))
    }
}
