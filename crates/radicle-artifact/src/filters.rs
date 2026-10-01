//! Visibility rules for releases and artifacts.

use crate::trust::Trust;
use crate::{Artifact, Cid, Release};

/// Visibility rules for releases and artifacts, as `list` / `show` apply them.
///
/// [`Trust`] decides whose releases and artifacts count. The flag below
/// opts into broader visibility; it defaults (at the caller) to hiding
/// withdrawn artifacts.
#[derive(Clone, Copy)]
pub struct Filters<'a> {
    /// Whose releases and artifacts are shown.
    pub trust: Trust<'a>,
    /// When true, include artifacts redacted by their author or by a delegate.
    pub include_redacted: bool,
}

impl<'a> Filters<'a> {
    /// Check whether `list` / `show` include `artifact`:
    /// - Artifacts redacted by a trusted party are hidden, unless
    ///   `include_redacted` is set.
    /// - Artifacts whose author `trust` does not trust are hidden.
    pub fn shows_artifact(&self, artifact: &Artifact) -> bool {
        if !self.include_redacted && artifact.is_redacted_by_trusted(self.trust.delegates) {
            return false;
        }
        // Delegates are the curated source of truth for a repo; non-delegate
        // contributions are opt-in. The local user always sees their own.
        self.trust.trusts(artifact.author())
    }

    /// The artifacts in `release` that [`Self::shows_artifact`] lets through.
    pub fn artifacts(self, release: &'a Release) -> impl Iterator<Item = (&'a Cid, &'a Artifact)> {
        release
            .artifacts()
            .iter()
            .filter(move |(_, artifact)| self.shows_artifact(artifact))
    }

    /// Check whether `list` shows `release`: `trust` must trust the creator,
    /// and a trusted party must not have redacted every artifact, unless
    /// `include_redacted` is set. Artifact authors do not matter here; they only
    /// decide which artifacts [`Self::shows_artifact`] lists.
    pub fn shows_release(&self, release: &Release) -> bool {
        self.trust.trusts(release.creator())
            && (self.include_redacted || !release.is_fully_redacted(self.trust.delegates))
    }
}
