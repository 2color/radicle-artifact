# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.11.0] - 2026-04-24

Small release fixing a regression introduced in `0.10.0` causing the the `serve` and `fetch` commands to fail creating an endpoint.

### Added

* `15f1c1b` add toy CI plan for Ambient to see if this can work at all *<liw@liw.fi>*

### Fixed

* `f7237fa` fix: iroh endpoint binding *<daniel@norman.life>*

### Other

* `7f6ddfa` build: chmod latest to 0644 before upload *<daniel@norman.life>*
* `0cfc62c` ci: add cargo fmt and test to ambient *<daniel@norman.life>*
* `d845dfc` chore: run cargo fmt *<daniel@norman.life>*

## [0.10.0] - 2026-04-24

This release brings a number of UX improvements to the `rad-artifact` cli, in addition to some improvements to the build and release process.

### ✨ Highlights

#### Streamlined artifact publishing with `rad-artifact add <PATH>`

You can now publish artifacts with `rad-artifact add <PATH>` and will be prompted to pick the commit/tag OID to which the artifact will be added. The CID is computed automatically, and if a release doesn't exist already, it will be created automatically.

You can still set the CID and commit/tag manually using `--cid` and `--commit`.

#### Delegate-only listing in `list` and `show` by default

`list` and `show` now hide non-delegate artifacts by default. The former `--delegates-only` flag is replaced by `--all-authors`, which widens the view back.

#### More informative pretty output in `rad-artifact list`

The artifact author's DID now sits alongside the CID of artifact so ownership is visible, and per-location rows are collapsed into a compact `scheme: count` summary so tables stay tight when an artifact is seeded from many endpoints.

### Added

* `4e571eb` Address cargo clippy warnings *<daniel@norman.life>*

### Other

* `15a6efa` Bump cid and multihash due to core2 getting yanked *<daniel@norman.life>*
* `db3a807` Bump iroh dependencies *<daniel@norman.life>*
* `8864895` cli: interactive add with path or CID source *<daniel@norman.life>*
* `bd11134` cli: show artifact author & location counts *<daniel@norman.life>*
* `0ed15ac` cli: default to only showing delegate artifacts *<daniel@norman.life>*
* `3b86140` docs: rewrite README intro *<daniel@norman.life>*
* `b48358d` cli: use commit OID in add examples *<daniel@norman.life>*
* `f7a4921` Bump radicle to 0.23.0 *<daniel@norman.life>*
* `bff8110` build: add install script *<daniel@norman.life>*
* `d7e0f06` build: update links to new radicle urls *<daniel@norman.life>*
* `818cdf7` build: fix make upload pre-check and docs drift *<daniel@norman.life>*
* `41900e2` build: support hand-written changelog notes *<daniel@norman.life>*
* `94857a2` build: add make changelog target *<daniel@norman.life>*
* `833150f` docs: update changelog *<daniel@norman.life>*
* `06be413` build: fix test compilation via radicle-oid qcheck *<daniel@norman.life>*


## [0.9.0] - 2026-04-21

### Other

* `7feb018` Improve README introduction *<daniel@norman.life>*
* `df344a4` Make release identity OID-only, not author-scoped *<daniel@norman.life>*
* `2bfdb35` Converge writes on duplicate releases per OID *<daniel@norman.life>*
* `26c4dfd` cli: add location to one release in serve cmd *<daniel@norman.life>*
* `69e1b20` Refine release pretty print output *<daniel@norman.life>*
* `f355e30` fetch: fail fast on missing or unreachable sources *<daniel@norman.life>*

## [0.8.0] - 2026-04-16

### Added

* `4a48ad2` Add rad-fetch CLI and fetch library *<daniel@norman.life>*
* `55dc5ee` Add artifact lookup helpers *<daniel@norman.life>*
* `c63045f` Add cid subcommand to rad-share *<daniel@norman.life>*
* `7c1a73c` Add list filtering and redaction hiding *<daniel@norman.life>*
* `cc4ff1a` Add commit titles to release display *<daniel@norman.life>*
* `ba087f1` Add --no-input flag and TTY check *<daniel@norman.life>*
* `4ca0640` Add examples to subcommand help text *<daniel@norman.life>*
* `70ce67f` Add --verbose to show and list commands *<daniel@norman.life>*
* `fd29aa3` Add interactive mode to attest and redact *<daniel@norman.life>*

### Changed

* `8b3bbe9` Rename fetch crate to share, add serving *<daniel@norman.life>*
* `89a05cb` Replace spaces with underscores in output name *<daniel@norman.life>*
* `31f4067` Change redact reason to --reason/-m flag *<daniel@norman.life>*
* `670afdd` Move release lookup methods to library *<daniel@norman.life>*

### Fixed

* `9589cf8` Fix iroh endpoint dropped during fetch *<daniel@norman.life>*
* `5eb4fc5` Fix failure to export after successful fetch *<daniel@norman.life>*
* `347e14b` Fix location remove failing for non-delegates *<daniel@norman.life>*
* `8eb89ed` Fix attest/redact failing for non-delegates *<daniel@norman.life>*
* `76e7cfa` Fix redacted/attested row over-indentation *<daniel@norman.life>*

### Other

* `066bf90` Use BLAKE3 CIDs and DID-derived endpoints *<daniel@norman.life>*
* `0c0d356` Show DID aliases in release display *<daniel@norman.life>*
* `707c620` Improve CLI help text and DID display *<daniel@norman.life>*
* `f4b8c92` Hide empty releases by default in list *<daniel@norman.life>*
* `523a543` Derive iroh endpoint ID from location DID *<daniel@norman.life>*
* `3036cb0` Include CID in default fetch output name *<daniel@norman.life>*
* `b2035fd` Improve fetch output and endpoint cleanup *<daniel@norman.life>*
* `589c853` Show short OID and commit title in picker *<daniel@norman.life>*
* `3bd45f3` Auto-create release COB on artifact add *<daniel@norman.life>*
* `8d848d5` Merge share crate into main crate *<daniel@norman.life>*
* `8fa325f` Print confirmation on mutating commands *<daniel@norman.life>*
* `f1a00d7` Auto-detect TTY for output format *<daniel@norman.life>*
* `f90fe10` Suggest next commands after mutations *<daniel@norman.life>*
* `11a0fb3` Color ERROR prefix red in terminal *<daniel@norman.life>*
* `4f4510e` Enforce fetch both-or-neither at parse time *<daniel@norman.life>*
* `5f5340f` Stream iroh downloads to disk with progress *<daniel@norman.life>*
* `c9155d3` cli: prefer flags in place of positional args *<daniel@norman.life>*
* `dd92c98` cli: accept shorthand commits via revparse *<daniel@norman.life>*
* `02e5bf2` Scope release lookups to delegates *<daniel@norman.life>*
* `89e5cb5` Improve list command output *<daniel@norman.life>*
* `751e3cc` Sort interactive fetch picker newest-first *<daniel@norman.life>*
* `ae39372` Prompt for passphrase on encrypted keys *<daniel@norman.life>*
* `30e26df` Narrow prompt module error types *<daniel@norman.life>*
* `044b306` Use inquire::Select for interactive picker *<daniel@norman.life>*
* `5f402ee` Render first & last 6 chars of cids in pretty mode *<daniel@norman.life>*
* `5b02745` Default endpoint preset to Radworks relay *<daniel@norman.life>*
* `142c49c` Refine serve command behavior *<daniel@norman.life>*
* `ed3e84c` Refine README *<daniel@norman.life>*
* `250d934` Use FsStore and rename cid module *<daniel@norman.life>*
* `704a56e` Stream hashing in compute_content_id *<daniel@norman.life>*
* `d8562e5` Release radicle-artifact version 0.8.0 *<daniel@norman.life>*

### Removed

* `5872d97` Drop tempfile runtime dep, use manual dirs *<daniel@norman.life>*
* `e1fb33b` Remove unused Fetcher trait *<daniel@norman.life>*

## [0.7.0] - 2026-03-25

### Other

* `a0d1544` Document how the COB is implemented *<daniel@norman.life>*
* `208f75b` Make self-attestation by artifact author no-op *<daniel@norman.life>*
* `4053ff6` Bump radicle crate to 0.22.1 *<daniel@norman.life>*
* `96251f1` Release radicle-artifact version 0.7.0 *<daniel@norman.life>*

## [0.6.0] - 2026-03-23

### Added

* `22ae509` Add redact for marking artifacts as compromised *<daniel@norman.life>*
* `e4f31a0` Add author to artifacts and allow name updates *<daniel@norman.life>*

### Changed

* `caf8d6c` Update README for artifact author and redactions *<daniel@norman.life>*

### Fixed

* `51ba100` Fix redact doc comment: max reason is 2048 bytes *<daniel@norman.life>*

### Other

* `16e4976` Document redact action in README *<daniel@norman.life>*
* `d675cd8` Clarify user vs. node and adapt explanations *<daniel@norman.life>*
* `6669b53` Prevent attestation after redaction for same DID *<daniel@norman.life>*
* `d0cd174` Release radicle-artifact version 0.6.0 *<daniel@norman.life>*

## [0.5.0] - 2026-03-19

### Changed

* `fe158fa` Rename NodeLocation to Location with did field *<daniel@norman.life>*

### Other

* `6951caa` Document the collaboration model *<daniel@norman.life>*
* `69c8717` Document build-system agnosticism *<daniel@norman.life>*
* `a42215c` Use Did instead of NodeId *<daniel@norman.life>*
* `ff843e7` Use user instead of node for consistency *<daniel@norman.life>*
* `871956b` Allow nodes to add multiple locations per artifact *<daniel@norman.life>*
* `6315178` Refine multiple locations and tighten tests *<daniel@norman.life>*
* `0d208d2` Stringify CIDs when rendering json *<daniel@norman.life>*
* `957f20f` Release radicle-artifact version 0.5.0 *<daniel@norman.life>*

## [0.4.0] - 2026-03-17

### Added

* `2d842fb` Add attestation support for artifact verification *<daniel@norman.life>*
* `c344c64` Add note about project in early development *<daniel@norman.life>*

### Changed

* `e2da22d` Update docs to reflect author and locations *<daniel@norman.life>*

### Other

* `f781788` Release radicle-artifact version 0.4.0 *<daniel@norman.life>*

## [0.3.0] - 2026-03-17

### Added

* `9d2ba99` Add author (NodeID) field to Release *<daniel@norman.life>*

### Other

* `f90b8ca` Simplify artifact locations to one URL per node *<daniel@norman.life>*
* `fe62047` Document release flag *<daniel@norman.life>*
* `dd160b5` Release radicle-artifact version 0.3.0 *<daniel@norman.life>*

## [0.2.0] - 2026-03-16

### Fixed

* `a363a05` Fix deprecated radicle 0.21.0 calls in announce *<daniel@norman.life>*

### Other

* `4882746` Bump radicle crate to 0.21.0 *<daniel@norman.life>*
* `789d7bb` Release radicle-artifact version 0.2.0 *<daniel@norman.life>*

## [0.1.0] - 2026-03-13

### Added

* `53182e9` Initial commit of radicle-artifact COB *<daniel@norman.life>*
* `6444e01` Add release tooling and changelog *<daniel@norman.life>*

### Changed

* `25b2061` Replace custom Cid wrapper with the cid crate *<daniel@norman.life>*

### Fixed

* `b443908` Fix iterator overflow, API safety, and add tests *<daniel@norman.life>*

### Other

* `32cb68f` Release radicle-artifact version 0.1.0 *<daniel@norman.life>*

