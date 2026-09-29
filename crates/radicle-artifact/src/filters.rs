//! Visibility rules for releases and artifacts.

use std::collections::BTreeSet;

use radicle::identity::Did;

/// Visibility rules for releases and artifacts, as `list` / `show` apply them
/// and as the `trust` checks reuse them.
///
/// Both filters below consult the repository's delegate set. The flags
/// opt into broader visibility; each defaults (at the caller) to a
/// delegate-scoped view.
#[derive(Clone, Copy)]
pub struct Filters<'a> {
    /// Delegates of the repository.
    pub delegates: &'a BTreeSet<Did>,
    /// When true, include artifacts redacted by their author or by a delegate.
    pub redacted: bool,
    /// When true, include artifacts whose author is not a repository delegate.
    pub all_authors: bool,
    /// Local user's DID. Artifacts authored by this user are always
    /// visible, even when the user isn't a delegate and `all_authors`
    /// is false — users should always see their own contributions.
    pub local: Option<&'a Did>,
}

impl Filters<'_> {
    /// Check whether `did` may create a release or register an artifact:
    /// a delegate, the local user, or anyone when `all_authors` is set.
    pub fn trusts(&self, did: &Did) -> bool {
        self.all_authors || self.delegates.contains(did) || self.local == Some(did)
    }
}

/// Check whether a redaction by `redactor` withdraws an artifact by `author`:
/// only the author and the delegates may withdraw it. `all_authors` does not
/// widen this — it opens up who may register an artifact, not who may
/// withdraw one.
pub fn honours_redaction(redactor: &Did, author: &Did, delegates: &BTreeSet<Did>) -> bool {
    redactor == author || delegates.contains(redactor)
}
