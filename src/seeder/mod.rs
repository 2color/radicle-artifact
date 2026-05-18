//! Seeder primitives: persistent iroh-blobs store, per-repo tag management,
//! import + size helpers.
//!
//! This module holds the bytes-and-tags layer of artifact seeding. It knows
//! nothing about COBs or signing — callers compose `seed()` with the
//! appropriate `add_location` write themselves.
//!
//! Currently exposes only [`keys`] for radicle ↔ iroh key conversion and
//! BASE32_NOPAD endpoint-id encoding. The bootstrap + tag + import
//! primitives land in a follow-up.

pub mod keys;
