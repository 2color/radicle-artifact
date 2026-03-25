# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.7.0] - 2026-03-25

### Other

* `a0d1544` Document how the COB is implemented *<daniel@norman.life>*
* `208f75b` Make self-attestation by artifact author no-op *<daniel@norman.life>*
* `4053ff6` Bump radicle crate to 0.22.1 *<daniel@norman.life>*

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

