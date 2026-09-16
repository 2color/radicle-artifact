//! Display forms of Release data.
//!
//! These can be used in tools that wish to display data, such as the
//! `rad-artifact` CLI tool.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use radicle::{git::Oid, identity::Did, node::AliasStore, storage::git::Repository};
use serde::Serialize;
use url::Url;

use crate::{Cid, ReleaseId};
use radicle_artifact_core::keys::EndpointId;
use radicle_artifact_core::protocol::FetchProgress;

/// A visible change to a progress display derived from a [`FetchProgress`]
/// frame: terminal frontends apply it to a spinner, but the type carries no
/// UI dependency of its own.
#[derive(Debug, PartialEq, Eq)]
pub enum ProgressUpdate {
    /// Replace the status message.
    Message(String),
    /// Advance the byte position.
    Position(u64),
}

/// Map a [`FetchProgress`] frame to a [`ProgressUpdate`], or `None` for frames
/// with no visible effect (a Location failing mid-try just rolls on to the
/// next).
///
/// Lives in the library so the match is exhaustive: a new `FetchProgress`
/// variant is a compile error here, not a silently dropped frame at a
/// frontend's `_ => {}`.
pub fn describe_progress(p: &FetchProgress) -> Option<ProgressUpdate> {
    match p {
        FetchProgress::Connecting => Some(ProgressUpdate::Message("connecting".into())),
        FetchProgress::TryingLocation { endpoint_id } => {
            Some(ProgressUpdate::Message(format!("trying {endpoint_id}")))
        }
        FetchProgress::LocationFailed { .. } => None,
        FetchProgress::Downloading { offset, .. } => Some(ProgressUpdate::Position(*offset)),
        FetchProgress::Exporting { .. } => Some(ProgressUpdate::Message("exporting".into())),
    }
}

/// Resolve a DID's alias via an [`AliasStore`], returning the string if found.
pub fn resolve(did: &Did, aliases: &impl AliasStore) -> Option<String> {
    aliases.alias(did.as_key()).map(|a| a.to_string())
}

/// Format a DID for display. The `did:key:` prefix is always stripped.
///
/// When `full` is false: keys are shown as first 7 + `…` + last 7.
/// When `full` is true the complete key is shown. In both cases an alias
/// is prepended as `alice@<key>` when available.
pub fn format_did(did: &Did, alias: &Option<String>, full: bool) -> String {
    let key = did.to_string().replace("did:key:", "");
    let displayed = if full {
        key
    } else {
        format!("{}…{}", &key[..7], &key[key.len() - 7..])
    };
    match alias {
        Some(alias) => format!("{alias}@{displayed}"),
        None => displayed,
    }
}

/// Format a byte count as a human-readable size (e.g. `1.5 MiB`). The raw
/// integer stays in JSON output; this is for the human-facing display only.
pub fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

/// Visible width of a string, ignoring ANSI SGR escape sequences and counting
/// each remaining `char` as a single column. Sufficient for the limited set of
/// characters used in our output (ASCII + a few BMP symbols like `…`, `●`, `▸`).
fn visible_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut width = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Skip CSI sequence up to and including the final byte (0x40-0x7e).
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i += 1;
            continue;
        }
        let b = bytes[i];
        let step = if b < 0x80 {
            1
        } else if b < 0xe0 {
            2
        } else if b < 0xf0 {
            3
        } else {
            4
        };
        i += step;
        width += 1;
    }
    width
}

/// Pad `s` on the right with spaces so its visible width is at least `target`.
fn pad_right(s: &str, target: usize) -> String {
    let w = visible_width(s);
    if w >= target {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + target - w);
        out.push_str(s);
        for _ in 0..(target - w) {
            out.push(' ');
        }
        out
    }
}

/// Append a line to a string buffer.
fn push_line(s: &mut String, line: String) {
    s.push_str(&line);
    s.push('\n');
}

/// Format rows as a column-aligned table, indented by `indent` spaces.
///
/// Computes column widths from all rows in a first pass, then emits each row
/// with cells padded to those widths. Trailing whitespace on each line is
/// trimmed. ANSI escapes are excluded from width calculation.
fn format_table(rows: &[Vec<String>], indent: usize) -> String {
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; ncols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(visible_width(cell));
        }
    }
    let prefix = " ".repeat(indent);
    let mut out = String::new();
    for row in rows {
        let mut line = prefix.clone();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 < row.len() {
                line.push_str(&pad_right(cell, widths[i]));
                line.push_str("  ");
            } else {
                line.push_str(cell);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Display style: knobs for verbosity and ANSI color output.
#[derive(Clone, Copy, Default)]
pub struct Style {
    /// When true, render full IDs/keys instead of truncating.
    pub verbose: bool,
    /// When true, emit ANSI color escapes; when false, plain text.
    pub color: bool,
}

impl Style {
    /// Style with the given verbosity and color enabled.
    pub fn colored(verbose: bool) -> Self {
        Self {
            verbose,
            color: true,
        }
    }

    /// Style with the given verbosity and color disabled.
    pub fn plain(verbose: bool) -> Self {
        Self {
            verbose,
            color: false,
        }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    fn magenta(&self, s: &str) -> String {
        self.paint("35", s)
    }
}

/// Resolve the first line of a release's title for display: the tag
/// message for tag-associated releases, the commit summary otherwise.
///
/// Returns `None` when the OID cannot be resolved (e.g. it lives in a
/// fork that hasn't been fetched).
pub trait CommitTitle {
    /// First line of `oid`'s message, where `oid` is a commit or an
    /// annotated tag object.
    fn title(&self, oid: &Oid) -> Option<String>;
}

/// No-op resolver that never produces a title.
impl CommitTitle for () {
    fn title(&self, _oid: &Oid) -> Option<String> {
        None
    }
}

/// For an annotated tag, returns the first non-empty line of the tag
/// message, falling back to the peeled commit's summary when the tag
/// has no message. For a commit, returns the commit summary.
impl CommitTitle for Repository {
    fn title(&self, oid: &Oid) -> Option<String> {
        let obj = self.backend.find_object((*oid).into(), None).ok()?;
        match obj.kind() {
            Some(radicle::git::raw::ObjectType::Tag) => {
                let tag = obj.as_tag()?;
                let from_tag = tag
                    .message()
                    .ok()
                    .flatten()
                    .and_then(|m| m.lines().next())
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from);
                from_tag.or_else(|| {
                    tag.target()
                        .ok()
                        .and_then(|t| t.peel(radicle::git::raw::ObjectType::Commit).ok())
                        .and_then(|c| c.into_commit().ok())
                        .and_then(|c| c.summary().ok().flatten().map(String::from))
                })
            }
            _ => obj
                .into_commit()
                .ok()
                .and_then(|c| c.summary().ok().flatten().map(String::from)),
        }
    }
}

/// Read the tag name out of an annotated-tag object (e.g. `v1.0`).
/// Returns `None` when the OID isn't an annotated-tag object or the
/// object isn't present locally.
pub trait TagName {
    /// Tag name embedded in the annotated-tag object at `tag_oid`.
    fn tag_name(&self, tag_oid: &Oid) -> Option<String>;
}

/// No-op resolver that never produces a tag name.
impl TagName for () {
    fn tag_name(&self, _tag_oid: &Oid) -> Option<String> {
        None
    }
}

/// Reads the name field from the annotated-tag object directly. The
/// COB stores the tag object's OID, so we don't need to scan refs.
impl TagName for Repository {
    fn tag_name(&self, tag_oid: &Oid) -> Option<String> {
        let obj = self.backend.find_object((*tag_oid).into(), None).ok()?;
        let tag = obj.as_tag()?;
        tag.name().ok().map(String::from)
    }
}

/// Visibility rules for artifacts rendered in `list` / `show` output.
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

/// A set of [`Release`]s sorted by creation time.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Releases {
    releases: Vec<Release>,
}

impl Releases {
    /// Construct the set of [`Releases`] given an iterator of [`ReleaseId`] and
    /// [`Release`][release] pairs.
    ///
    /// The `aliases` store is used to resolve human-readable aliases for DIDs.
    ///
    /// `filters` controls artifact visibility — see [`Filters`] for the
    /// redaction and author-trust knobs. When `show_empty` is false,
    /// releases with no visible artifacts are excluded from the output.
    ///
    /// The `titles` resolver looks up the title line for each release's
    /// keying ref — the tag message when the release records a tag,
    /// otherwise the commit summary. `tag_names` resolves the
    /// annotated-tag object back to its `v1.0`-style name for display.
    ///
    /// [release]: crate::Release
    pub fn new(
        releases: impl Iterator<Item = (ReleaseId, crate::Release)>,
        aliases: &impl AliasStore,
        filters: Filters<'_>,
        show_empty: bool,
        titles: &impl CommitTitle,
        tag_names: &impl TagName,
    ) -> Self {
        let mut releases: Vec<_> = releases
            .map(|(id, release)| {
                // Prefer the tag's title when set; fall back to the commit
                // summary if the tag object isn't present locally.
                let title = release
                    .tag()
                    .and_then(|t| titles.title(t))
                    .or_else(|| titles.title(release.oid()));
                let tag_name = release.tag().and_then(|t| tag_names.tag_name(t));
                Release::new(id, &release, aliases, filters, title, tag_name)
            })
            .filter(|r| show_empty || !r.artifacts.is_empty())
            .collect();
        releases.sort_by_key(|r| Reverse(r.created_at));

        Self { releases }
    }

    /// Pretty print as a compact list, suitable for the `list` command.
    /// Releases are separated by a dim rule; a summary footer reports
    /// total release and artifact counts.
    pub fn pretty(&self, style: Style) -> String {
        let mut s = String::new();
        let total_artifacts: usize = self.releases.iter().map(|r| r.artifacts.len()).sum();

        for (i, shown) in self.releases.iter().enumerate() {
            if i > 0 {
                push_line(&mut s, style.dim("─────"));
            }
            s.push_str(&shown.pretty_compact(style));
        }

        if !self.releases.is_empty() {
            s.push('\n');
        }
        let summary = format!(
            "{} {}, {} {}",
            self.releases.len(),
            if self.releases.len() == 1 {
                "release"
            } else {
                "releases"
            },
            total_artifacts,
            if total_artifacts == 1 {
                "artifact"
            } else {
                "artifacts"
            },
        );
        push_line(&mut s, style.dim(&summary));
        s
    }

    /// Pretty print as a sequence of detailed release blocks, suitable
    /// for the `show` command. Multiple releases (e.g. when showing all
    /// releases for a revision) are separated by a dim rule.
    pub fn pretty_detailed(&self, style: Style) -> String {
        let mut s = String::new();
        for (i, shown) in self.releases.iter().enumerate() {
            if i > 0 {
                push_line(&mut s, style.dim("─────"));
            }
            s.push_str(&shown.pretty(style));
        }
        s
    }

    /// Consume self and return the inner [`Release`]s, e.g. for
    /// callers that need a bare-array JSON shape.
    pub fn into_inner(self) -> Vec<Release> {
        self.releases
    }
}

/// A display form of a [`Release`][release].
///
/// [release]: crate::Release
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    release_id: ReleaseId,
    /// Unix seconds when this release COB was created.
    created_at: u64,
    /// Commit OID this release is keyed by.
    oid: Oid,
    /// Annotated tag OID when this release is associated with a tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<Oid>,
    /// Annotated tag name (e.g. `v1.0`), resolved from the tag object.
    /// `None` when the release has no tag, or when the tag object isn't
    /// present locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_name: Option<String>,
    /// First line of the tag or commit message. Locally-resolved; not
    /// persisted in the COB.
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// DID of the user who authored the release COB.
    creator: Did,
    /// Alias for [`Self::creator`] when known to the local node.
    #[serde(skip_serializing_if = "Option::is_none")]
    creator_alias: Option<String>,
    artifacts: Vec<Artifact>,
    /// Local user's DID, used to derive the 🌱 seeding badge at render
    /// time. Not serialized — JSON consumers can compute seeding from
    /// `locations` and their own DID.
    #[serde(skip)]
    local: Option<Did>,
}

impl Release {
    /// Construct a new [`Release`] display form.
    ///
    /// The `aliases` store is used to resolve human-readable aliases for DIDs.
    /// `filters` controls which artifacts are included: by default artifacts
    /// redacted by the author or a delegate are hidden, and artifacts whose
    /// author is not a delegate are hidden. Set the corresponding
    /// [`Filters`] flags to `true` to include them.
    ///
    /// `title` is the first line of the tag or commit message, if
    /// available — see [`CommitTitle`]. `tag_name` is the annotated
    /// tag's name when one is associated and resolvable locally — see
    /// [`TagName`].
    pub fn new(
        release_id: ReleaseId,
        release: &crate::Release,
        aliases: &impl AliasStore,
        filters: Filters<'_>,
        title: Option<String>,
        tag_name: Option<String>,
    ) -> Self {
        let mut artifacts: Vec<_> = release
            .artifacts()
            .iter()
            .filter(|(_cid, artifact)| {
                // Redaction filter: hide artifacts redacted by a trusted
                // party (the author itself or any repository delegate).
                if !filters.redacted {
                    let hidden = artifact
                        .redactions()
                        .keys()
                        .any(|did| *did == *artifact.author() || filters.delegates.contains(did));
                    if hidden {
                        return false;
                    }
                }
                // Author filter: hide artifacts added by users who are not
                // repository delegates. Delegates are the curated source of
                // truth for a repo; non-delegate contributions are opt-in.
                // The local user is always exempt so they can see their own
                // contributions without `--all-authors`.
                if !filters.all_authors
                    && !filters.delegates.contains(artifact.author())
                    && filters.local != Some(artifact.author())
                {
                    return false;
                }
                true
            })
            .map(|(cid, artifact)| {
                let mut locations: Vec<_> = artifact
                    .locations()
                    .iter()
                    .flat_map(|(did, urls)| {
                        let alias = resolve(did, aliases);
                        urls.iter().map(move |url| Location {
                            alias: alias.clone(),
                            did: *did,
                            url: url.clone(),
                        })
                    })
                    .collect();
                // Sort by (did, url) for deterministic output.
                locations
                    .sort_by(|a, b| a.did.cmp(&b.did).then(a.url.as_str().cmp(b.url.as_str())));
                let attestations: Vec<_> = artifact
                    .attestations()
                    .iter()
                    .map(|did| Attestation {
                        alias: resolve(did, aliases),
                        did: *did,
                    })
                    .collect();
                let mut redactions: Vec<_> = artifact
                    .redactions()
                    .iter()
                    .map(|(did, reason)| Redaction {
                        alias: resolve(did, aliases),
                        did: *did,
                        reason: reason.clone(),
                    })
                    .collect();
                // Sort by DID for deterministic output.
                redactions.sort_by_key(|a| a.did);
                let artifact_author = *artifact.author();
                Artifact {
                    cid: cid.to_string(),
                    author_alias: resolve(&artifact_author, aliases),
                    author: artifact_author,
                    name: artifact.name().to_owned(),
                    locations,
                    attestations,
                    redactions,
                    metadata: artifact.metadata().clone(),
                }
            })
            .collect();
        // Sort artifacts by CID string for deterministic output.
        artifacts.sort_by(|a, b| a.cid.cmp(&b.cid));

        let creator = *release.creator();
        Self {
            release_id,
            created_at: release.timestamp().as_secs(),
            oid: *release.oid(),
            tag: release.tag().copied(),
            tag_name,
            title,
            creator,
            creator_alias: resolve(&creator, aliases),
            artifacts,
            local: filters.local.copied(),
        }
    }

    /// Whether the local user advertises itself as a provider for this
    /// artifact: it carries a `radiroh://` location under our own DID
    /// that resolves to our endpoint id — computed identically to how
    /// any peer decides we're a provider. See [`EndpointId::matches_url`].
    fn seeding(&self, artifact: &Artifact) -> bool {
        self.local.is_some_and(|local| {
            EndpointId::try_from(&local).is_ok_and(|me| {
                artifact
                    .locations
                    .iter()
                    .any(|l| l.did == local && me.matches_url(&l.url))
            })
        })
    }

    /// Build the colored release header line: bullet, short id, ref
    /// label (tag/commit), creator, optional title.
    fn header(&self, style: Style) -> String {
        let id_str = if style.verbose {
            self.release_id.to_string()
        } else {
            self.release_id.to_string()[..7].to_string()
        };
        let oid_full = self.oid.to_string();
        let oid_str = if style.verbose {
            oid_full.clone()
        } else {
            oid_full[..7].to_string()
        };

        // tag {name -or- short_oid} → commit {oid}; or just commit {oid}.
        let ref_label = match (&self.tag_name, &self.tag) {
            (Some(name), _) => format!(
                "{} {} {} {} {}",
                style.dim("tag"),
                style.bold(name),
                style.dim("→"),
                style.dim("commit"),
                style.yellow(&oid_str),
            ),
            (None, Some(tag_oid)) => {
                let tag_full = tag_oid.to_string();
                let short_tag = if style.verbose {
                    tag_full.clone()
                } else {
                    tag_full[..7].to_string()
                };
                format!(
                    "{} {} {} {} {}",
                    style.dim("tag"),
                    style.yellow(&short_tag),
                    style.dim("→"),
                    style.dim("commit"),
                    style.yellow(&oid_str),
                )
            }
            (None, None) => format!("{} {}", style.dim("commit"), style.yellow(&oid_str)),
        };

        let creator = format_did(&self.creator, &self.creator_alias, style.verbose);
        let title = match &self.title {
            Some(t) if !t.is_empty() => format!("  {t}"),
            _ => String::new(),
        };
        format!(
            "{} {}  {}  {} {}{}",
            style.cyan("●"),
            style.bold(&id_str),
            ref_label,
            style.dim("by"),
            style.dim(&creator),
            title,
        )
    }

    /// Pretty print a release in compact form, suitable for `list`.
    ///
    /// Each artifact renders as one row: cid, name, author, locations
    /// (scheme-summarised), and attestation/redaction badges. CIDs render
    /// in full so they can be copy-pasted without `--verbose`.
    pub fn pretty_compact(&self, style: Style) -> String {
        let mut s = String::new();
        push_line(&mut s, self.header(style));

        if self.artifacts.is_empty() {
            push_line(&mut s, format!("  {}", style.dim("(no artifacts)")));
            return s;
        }

        let mut rows: Vec<Vec<String>> = Vec::new();
        for artifact in self.artifacts.iter() {
            // CIDs always render in full so they can be copy-pasted without
            // re-running with --verbose; only DIDs are truncated.
            let cid_cell = artifact.cid.clone();
            let author = format_did(&artifact.author, &artifact.author_alias, style.verbose);

            let locations_cell = if style.verbose {
                let mut counts: std::collections::BTreeMap<&str, usize> =
                    std::collections::BTreeMap::new();
                for loc in artifact.locations.iter() {
                    *counts.entry(loc.url.scheme()).or_insert(0) += 1;
                }
                counts
                    .iter()
                    .map(|(scheme, count)| format!("{scheme}: {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                format!("({} 📍)", artifact.locations.len())
            };

            let seed = if self.seeding(artifact) {
                "🌱".to_string()
            } else {
                String::new()
            };
            let mut badges = String::new();
            if !artifact.attestations.is_empty() {
                badges.push_str(&style.green(&format!("✓{}", artifact.attestations.len())));
            }
            if !artifact.redactions.is_empty() {
                if !badges.is_empty() {
                    badges.push(' ');
                }
                badges.push_str(&style.red(&format!("⊘{}", artifact.redactions.len())));
            }

            rows.push(vec![
                style.magenta(&cid_cell),
                style.bold(&artifact.name),
                style.dim(&author),
                seed,
                locations_cell,
                badges,
            ]);
        }
        s.push_str(&format_table(&rows, 2));
        s
    }

    /// Pretty print a release in detailed form, suitable for `show`.
    ///
    /// Lays out each artifact as a labeled block with one field per line,
    /// so individual fields are easy to scan and copy. In verbose mode
    /// the full release-id and commit OID are surfaced as their own
    /// lines for copy/paste.
    pub fn pretty(&self, style: Style) -> String {
        let mut s = String::new();
        push_line(&mut s, self.header(style));

        if style.verbose {
            let label = |k: &str| pad_right(&style.dim(k), 14);
            push_line(&mut s, format!("  {}{}", label("release"), self.release_id));
            push_line(&mut s, format!("  {}{}", label("commit"), self.oid));
            if let Some(tag_oid) = self.tag {
                push_line(&mut s, format!("  {}{}", label("tag"), tag_oid));
            }
        }

        if self.artifacts.is_empty() {
            s.push('\n');
            push_line(&mut s, format!("  {}", style.dim("(no artifacts)")));
            return s;
        }

        s.push('\n');
        let count = self.artifacts.len();
        let heading = format!(
            "Artifacts ({count} {})",
            if count == 1 { "item" } else { "items" }
        );
        push_line(&mut s, format!("  {}", style.bold(&heading)));

        for artifact in self.artifacts.iter() {
            s.push('\n');
            // Artifact heading: name in bold with badges.
            let mut badges = String::new();
            if self.seeding(artifact) {
                badges.push_str(" 🌱");
            }
            if !artifact.attestations.is_empty() {
                badges.push_str(&style.green(&format!(" ✓{}", artifact.attestations.len())));
            }
            if !artifact.redactions.is_empty() {
                badges.push_str(&style.red(&format!(" ⊘{}", artifact.redactions.len())));
            }
            push_line(
                &mut s,
                format!(
                    "  {} {}{}",
                    style.cyan("▸"),
                    style.bold(&artifact.name),
                    badges
                ),
            );

            // Padded labels for value lines; bare labels for nested
            // blocks (locations/attestations/redactions) so trailing
            // whitespace isn't emitted.
            let label = |k: &str| pad_right(&style.dim(k), 14);
            let bare_label = |k: &str| style.dim(k);
            // Always show the full CID in detailed view too.
            push_line(
                &mut s,
                format!("    {}{}", label("cid"), style.magenta(&artifact.cid)),
            );
            let author = format_did(&artifact.author, &artifact.author_alias, style.verbose);
            push_line(&mut s, format!("    {}{}", label("author"), author));

            if artifact.locations.is_empty() {
                push_line(
                    &mut s,
                    format!("    {}{}", label("locations"), style.dim("(none)")),
                );
            } else {
                push_line(&mut s, format!("    {}", bare_label("locations")));
                // Group locations by DID so each provider is one block.
                let mut by_did: indexmap::IndexMap<Did, (Option<String>, Vec<&Url>)> =
                    indexmap::IndexMap::new();
                for loc in artifact.locations.iter() {
                    by_did
                        .entry(loc.did)
                        .or_insert_with(|| (loc.alias.clone(), Vec::new()))
                        .1
                        .push(&loc.url);
                }
                for (did, (alias, urls)) in by_did {
                    let provider = format_did(&did, &alias, style.verbose);
                    push_line(&mut s, format!("      {}", style.dim(&provider)));
                    for url in urls {
                        push_line(&mut s, format!("        {url}"));
                    }
                }
            }

            if !artifact.attestations.is_empty() {
                push_line(&mut s, format!("    {}", bare_label("attestations")));
                for a in artifact.attestations.iter() {
                    let did = format_did(&a.did, &a.alias, style.verbose);
                    push_line(&mut s, format!("      {} {did}", style.green("✓")));
                }
            }
            if !artifact.redactions.is_empty() {
                push_line(&mut s, format!("    {}", bare_label("redactions")));
                for r in artifact.redactions.iter() {
                    let did = format_did(&r.did, &r.alias, style.verbose);
                    let reason = if r.reason.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", style.dim(&r.reason))
                    };
                    push_line(&mut s, format!("      {} {did}{reason}", style.red("⊘")));
                }
            }
            if !artifact.metadata.is_empty() {
                push_line(&mut s, format!("    {}", bare_label("metadata")));
                for (key, value) in artifact.metadata.iter() {
                    // The size hint renders human-friendly (the raw integer
                    // stays in --json). Strings render unquoted to keep simple
                    // notes readable; other JSON shapes render as compact JSON.
                    let rendered = match (key.as_str(), value) {
                        (crate::METADATA_KEY_SIZE_BYTES, serde_json::Value::Number(n))
                            if n.is_u64() =>
                        {
                            human_bytes(n.as_u64().expect("checked is_u64"))
                        }
                        (_, serde_json::Value::String(s)) => s.clone(),
                        (_, other) => other.to_string(),
                    };
                    push_line(&mut s, format!("      {} = {}", style.cyan(key), rendered));
                }
            }
        }
        s
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Artifact {
    cid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_alias: Option<String>,
    author: Did,
    name: String,
    locations: Vec<Location>,
    attestations: Vec<Attestation>,
    redactions: Vec<Redaction>,
    // Always emitted, empty when unset, like the sibling collections
    // (locations/attestations/redactions); see docs/adr/0001-json-casing.md.
    metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
    url: Url,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Attestation {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Redaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
    reason: String,
}

/// `create --json` payload: the release that was created or reused. Named
/// like the wire `*Receipt` command results in `radicle-artifact-core`.
///
/// camelCase keys via `rename_all`, like the other output forms here; see
/// `docs/adr/0001-json-casing.md`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReceipt {
    release_id: ReleaseId,
    oid: Oid,
}

impl CreateReceipt {
    /// Build the `create --json` payload from the created/reused release id.
    pub fn new(release_id: ReleaseId, oid: Oid) -> Self {
        Self { release_id, oid }
    }
}

/// `register --json` payload: the registered artifact and its release.
///
/// A recorded size hint nests under `metadata` (keys verbatim, matching
/// `list`/`show`); `metadata` is always present, empty when no size was
/// recorded. See `docs/adr/0001-json-casing.md`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterReceipt {
    cid: Cid,
    name: String,
    release_id: ReleaseId,
    oid: Oid,
    metadata: BTreeMap<String, serde_json::Value>,
}

impl RegisterReceipt {
    /// Build the `register --json` payload; `size` is the byte hint recorded
    /// on the artifact, or `None` when registering by `--cid`. `name` tells
    /// the lines apart when one `register` writes several artifacts.
    pub fn new(cid: Cid, name: String, release_id: ReleaseId, oid: Oid, size: Option<u64>) -> Self {
        let mut metadata = BTreeMap::new();
        if let Some(bytes) = size {
            // Keyed by the stored COB metadata key so the output field
            // stays coupled to what `register` actually wrote.
            metadata.insert(
                crate::METADATA_KEY_SIZE_BYTES.to_string(),
                serde_json::json!(bytes),
            );
        }
        Self {
            cid,
            name,
            release_id,
            oid,
            metadata,
        }
    }
}

/// `verify --json` payload: the CID computed from the local file, and every
/// release that registers it.
///
/// `matches` is always non-empty — a failed verification exits non-zero
/// instead of emitting a receipt. See `docs/adr/0001-json-casing.md`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReceipt {
    cid: Cid,
    /// Always `true`, so one field answers the question for this payload
    /// and for [`VerifyFailure`] alike.
    verified: bool,
    matches: Vec<VerifyMatch>,
}

impl VerifyReceipt {
    /// Build the `verify` payload from the computed CID and its matches.
    pub fn new(cid: Cid, matches: Vec<VerifyMatch>) -> Self {
        Self {
            cid,
            verified: true,
            matches,
        }
    }

    /// Pretty print what registered the verified bytes, one labeled field
    /// per line to match `show`'s artifact blocks.
    pub fn pretty(&self, style: Style) -> String {
        let mut s = String::new();
        push_line(
            &mut s,
            format!("{} {}", style.green("✓ Verified"), self.cid),
        );
        let label = |k: &str| pad_right(&style.dim(k), 12);
        for m in self.matches.iter() {
            s.push('\n');
            let release = match &m.tag_name {
                Some(tag) => format!("{}  ({tag})", m.release_id),
                None => m.release_id.to_string(),
            };
            push_line(&mut s, format!("  {}{release}", label("release")));
            push_line(&mut s, format!("  {}{}", label("artifact"), m.name));
            let mut author = match &m.author_alias {
                Some(alias) => format!("{} ({alias})", m.author),
                None => m.author.to_string(),
            };
            if m.delegate {
                author.push_str(&format!(" {}", style.dim("delegate")));
            }
            push_line(&mut s, format!("  {}{author}", label("author")));
            push_line(
                &mut s,
                format!("  {}{}", label("attested"), m.attestations.len()),
            );
        }
        s
    }
}

/// `verify --json` payload for a negative verdict: the bytes were checked
/// and are not trustworthy.
///
/// Printed beside the non-zero exit, so a caller that asked for JSON gets
/// the reason as data rather than only a status code. A failure that could
/// not answer the question at all emits no payload — there is no verdict.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyFailure {
    cid: Cid,
    /// Always `false`. See [`VerifyReceipt::verified`].
    verified: bool,
    /// A stable token: `noMatch`, `redacted` or `untrustedAuthor`.
    reason: &'static str,
    /// The same sentence that goes to stderr.
    message: String,
    /// Who withdrew the artifact, and why, when `reason` is `redacted`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    redactions: BTreeMap<Did, String>,
}

impl VerifyFailure {
    /// Build the payload from the computed CID and the rejection.
    pub fn new(
        cid: Cid,
        reason: &'static str,
        message: String,
        redactions: BTreeMap<Did, String>,
    ) -> Self {
        Self {
            cid,
            verified: false,
            reason,
            message,
            redactions,
        }
    }
}

/// One release that registers the verified CID.
///
/// The same CID can appear in several releases, so `verify` reports each
/// one rather than choosing between them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyMatch {
    release_id: ReleaseId,
    oid: Oid,
    /// Annotated tag name (e.g. `releases/1.0.0`), when the release records
    /// a tag and the tag object is present locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_name: Option<String>,
    name: String,
    author: Did,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_alias: Option<String>,
    /// Whether the artifact's author is a repository delegate. When false,
    /// the match was accepted because the author is the local user or
    /// because `--all-authors` was passed.
    delegate: bool,
    /// DIDs that recorded an attestation for this artifact.
    attestations: Vec<Did>,
}

impl VerifyMatch {
    /// Build a match from a release and the artifact within it carrying the
    /// verified CID. `delegate` says whether that artifact's author is a
    /// repository delegate.
    pub fn new(
        release_id: ReleaseId,
        release: &crate::Release,
        artifact: &crate::Artifact,
        aliases: &impl AliasStore,
        tag_names: &impl TagName,
        delegate: bool,
    ) -> Self {
        Self {
            release_id,
            oid: *release.oid(),
            tag_name: release.tag().and_then(|oid| tag_names.tag_name(oid)),
            name: artifact.name().to_owned(),
            author: *artifact.author(),
            author_alias: resolve(artifact.author(), aliases),
            delegate,
            attestations: artifact.attestations().iter().copied().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// A valid CIDv1 (raw codec, sha2-256) for output-shape assertions.
    fn test_cid() -> Cid {
        use cid::multihash::Multihash;
        let mh = Multihash::<64>::wrap(0x12, &[0u8; 32]).unwrap();
        Cid::from(cid::Cid::new_v1(0x55, mh))
    }

    fn test_oid() -> Oid {
        Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap()
    }

    #[test]
    fn create_receipt_is_camelcase() {
        let oid = test_oid();
        let release_id = ReleaseId::from(oid);
        let out = CreateReceipt::new(release_id, oid);
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            serde_json::json!({"releaseId": release_id.to_string(), "oid": oid.to_string()})
        );
    }

    #[test]
    fn register_receipt_nests_size_under_metadata() {
        let (cid, oid) = (test_cid(), test_oid());
        let release_id = ReleaseId::from(oid);
        let out =
            RegisterReceipt::new(cid, "my-binary".to_string(), release_id, oid, Some(1048576));
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            serde_json::json!({
                "cid": cid.to_string(),
                "name": "my-binary",
                "releaseId": release_id.to_string(),
                "oid": oid.to_string(),
                // camelCase system metadata key, nested under `metadata`.
                "metadata": {"sizeBytes": 1048576},
            })
        );
    }

    #[test]
    fn register_receipt_emits_empty_metadata_when_no_size() {
        let (cid, oid) = (test_cid(), test_oid());
        let release_id = ReleaseId::from(oid);
        let out = RegisterReceipt::new(cid, "my-binary".to_string(), release_id, oid, None);
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            serde_json::json!({
                "cid": cid.to_string(),
                "name": "my-binary",
                "releaseId": release_id.to_string(),
                "oid": oid.to_string(),
                // Always present so the shape is stable, empty when no size.
                "metadata": {},
            })
        );
    }

    /// The `--json` display forms use camelCase structural keys, while
    /// metadata keys (user- and system-defined) pass through verbatim.
    /// Locks the convention in `docs/adr/0001-json-casing.md`.
    #[test]
    fn release_json_is_camelcase_with_verbatim_metadata() {
        let did =
            Did::from_str("did:key:z6MkiTBz1ymuepAQ4HEHYSF1H8quG5GLVVQR3djdX3mDooWp").unwrap();
        let oid = Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap();
        let release_id = ReleaseId::from(oid);
        let url = Url::parse("https://example.com/a").unwrap();

        let mut metadata = BTreeMap::new();
        metadata.insert(
            crate::METADATA_KEY_SIZE_BYTES.to_string(),
            serde_json::json!(1048576),
        );
        metadata.insert("user-note".to_string(), serde_json::json!("hello"));

        let artifact = Artifact {
            cid: "bafybeigdyrartifactcid".to_string(),
            author_alias: Some("alice".to_string()),
            author: did,
            name: "app.bin".to_string(),
            locations: vec![Location {
                alias: Some("alice".to_string()),
                did,
                url: url.clone(),
            }],
            attestations: vec![Attestation { alias: None, did }],
            redactions: vec![Redaction {
                alias: None,
                did,
                reason: "superseded".to_string(),
            }],
            metadata,
        };
        let release = Release {
            release_id,
            created_at: 1_700_000_000,
            oid,
            tag: Some(oid),
            tag_name: Some("v1.0".to_string()),
            title: Some("Release 1".to_string()),
            creator: did,
            creator_alias: Some("alice".to_string()),
            artifacts: vec![artifact],
            local: None,
        };

        assert_eq!(
            serde_json::to_value(&release).unwrap(),
            serde_json::json!({
                "releaseId": release_id.to_string(),
                "createdAt": 1_700_000_000,
                "oid": oid.to_string(),
                "tag": oid.to_string(),
                "tagName": "v1.0",
                "title": "Release 1",
                "creator": did.to_string(),
                "creatorAlias": "alice",
                "artifacts": [{
                    "cid": "bafybeigdyrartifactcid",
                    "authorAlias": "alice",
                    "author": did.to_string(),
                    "name": "app.bin",
                    "locations": [{"alias": "alice", "did": did.to_string(), "url": "https://example.com/a"}],
                    "attestations": [{"did": did.to_string()}],
                    "redactions": [{"did": did.to_string(), "reason": "superseded"}],
                    // System key camelCase; user key verbatim.
                    "metadata": {"sizeBytes": 1048576, "user-note": "hello"},
                }],
            })
        );
    }

    /// `describe_progress` maps every `FetchProgress` arm to its display
    /// intent, and collapses the no-op Location-failure frame to `None`.
    #[test]
    fn describe_progress_maps_every_frame() {
        let endpoint_id = EndpointId::from(iroh_base::SecretKey::from_bytes(&[1u8; 32]).public());

        assert_eq!(
            describe_progress(&FetchProgress::Connecting),
            Some(ProgressUpdate::Message("connecting".into()))
        );
        assert_eq!(
            describe_progress(&FetchProgress::TryingLocation { endpoint_id }),
            Some(ProgressUpdate::Message(format!("trying {endpoint_id}")))
        );
        assert_eq!(
            describe_progress(&FetchProgress::LocationFailed { endpoint_id }),
            None
        );
        assert_eq!(
            describe_progress(&FetchProgress::Downloading {
                offset: 4096,
                total: Some(8192),
            }),
            Some(ProgressUpdate::Position(4096))
        );
        assert_eq!(
            describe_progress(&FetchProgress::Exporting {
                offset: 1,
                total: None,
                entry: None,
            }),
            Some(ProgressUpdate::Message("exporting".into()))
        );
    }

    #[test]
    fn human_bytes_picks_unit_by_threshold() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024 * 3 / 2), "1.50 GiB");
    }
}
