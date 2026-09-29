//! Visibility rules for releases and artifacts.

use crate::trust::Trust;
use crate::{Artifact, Release};

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
    pub redacted: bool,
}

impl Filters<'_> {
    /// Check whether `list` / `show` include `artifact`:
    /// - Artifacts redacted by a trusted party are hidden, unless
    ///   `redacted` is set.
    /// - Artifacts whose author `trust` does not trust are hidden.
    pub fn shows_artifact(&self, artifact: &Artifact) -> bool {
        if !self.redacted && artifact.is_redacted_by_trusted(self.trust.delegates) {
            return false;
        }
        // Delegates are the curated source of truth for a repo; non-delegate
        // contributions are opt-in. The local user always sees their own.
        self.trust.trusts(artifact.author())
    }

    /// Check whether `list` shows `release`.
    ///
    /// `trust` must trust the release creator. Then a release with no
    /// artifacts is shown, and any other release is shown when at least one
    /// artifact passes [`Self::shows_artifact`].
    pub fn shows_release(&self, release: &Release) -> bool {
        self.trust.trusts(release.creator())
            && (release.artifacts().is_empty()
                || release.artifacts().values().any(|a| self.shows_artifact(a)))
    }
}
