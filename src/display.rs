//! Display forms of Release data.
//!
//! These can be used in tools that wish to display data, such as the
//! `rad-artifact` CLI tool.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use radicle::{git::Oid, identity::Did, node::AliasStore, storage::git::Repository};
use serde::Serialize;
use url::Url;

use crate::ReleaseId;

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
                    .and_then(|m| m.lines().next())
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from);
                from_tag.or_else(|| {
                    tag.target()
                        .ok()
                        .and_then(|t| t.peel(radicle::git::raw::ObjectType::Commit).ok())
                        .and_then(|c| c.into_commit().ok())
                        .and_then(|c| c.summary().map(String::from))
                })
            }
            _ => obj
                .into_commit()
                .ok()
                .and_then(|c| c.summary().map(String::from)),
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
        tag.name().map(String::from)
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
                redactions.sort_by(|a, b| a.did.cmp(&b.did));
                let artifact_author = *artifact.author();
                Artifact {
                    cid: cid.to_string(),
                    author_alias: resolve(&artifact_author, aliases),
                    author: artifact_author,
                    name: artifact.name().to_owned(),
                    locations,
                    attestations,
                    redactions,
                }
            })
            .collect();
        // Sort artifacts by CID string for deterministic output.
        artifacts.sort_by(|a, b| a.cid.cmp(&b.cid));

        let creator = *release.creator();
        Self {
            release_id,
            created_at: release.timestamp(),
            oid: *release.oid(),
            tag: release.tag().copied(),
            tag_name,
            title,
            creator,
            creator_alias: resolve(&creator, aliases),
            artifacts,
        }
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
    /// (scheme-summarised), and attestation/redaction badges.
    pub fn pretty_compact(&self, style: Style) -> String {
        let mut s = String::new();
        push_line(&mut s, self.header(style));

        if self.artifacts.is_empty() {
            push_line(&mut s, format!("  {}", style.dim("(no artifacts)")));
            return s;
        }

        let mut rows: Vec<Vec<String>> = Vec::new();
        for artifact in self.artifacts.iter() {
            let cid_cell = if style.verbose {
                artifact.cid.clone()
            } else {
                format!(
                    "{}…{}",
                    &artifact.cid[..6],
                    &artifact.cid[artifact.cid.len() - 6..]
                )
            };
            let author = format_did(&artifact.author, &artifact.author_alias, style.verbose);

            // Summarise location counts by URL scheme to keep the row compact.
            let mut scheme_counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for loc in artifact.locations.iter() {
                *scheme_counts.entry(loc.url.scheme()).or_insert(0) += 1;
            }
            let locations_cell = scheme_counts
                .iter()
                .map(|(scheme, count)| format!("{scheme}: {count}"))
                .collect::<Vec<_>>()
                .join(", ");

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
            push_line(
                &mut s,
                format!("  {}{}", label("release"), self.release_id),
            );
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
            let cid_str = if style.verbose {
                artifact.cid.clone()
            } else {
                format!(
                    "{}…{}",
                    &artifact.cid[..6],
                    &artifact.cid[artifact.cid.len() - 6..]
                )
            };
            push_line(
                &mut s,
                format!("    {}{}", label("cid"), style.magenta(&cid_str)),
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
        }
        s
    }
}

#[derive(Serialize)]
struct Artifact {
    cid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_alias: Option<String>,
    author: Did,
    name: String,
    locations: Vec<Location>,
    attestations: Vec<Attestation>,
    redactions: Vec<Redaction>,
}

#[derive(Serialize)]
struct Location {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
    url: Url,
}

#[derive(Serialize)]
struct Attestation {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
}

#[derive(Serialize)]
struct Redaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    did: Did,
    reason: String,
}
