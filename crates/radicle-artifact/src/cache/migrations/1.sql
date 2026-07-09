-- Initial schema for the artifact Release COB cache.

-- Materialized releases, one row per Release COB, keyed by object id.
create table if not exists "releases" (
  -- Release (Object) ID, hex OID.
  "id"        text primary key not null,
  -- Repository ID (URN).
  "repo"      text not null,
  -- Freshness token: sorted, joined tip OIDs of the COB's change graph.
  -- A read compares this against the current tips to decide if the row is stale.
  "head"      text not null,
  -- The materialized Release as JSON.
  "release"   text not null
) strict;

create index if not exists "ix_releases_repo" on "releases" (repo, id);

-- Normalized index for CID lookups. The CID is a dynamic JSON object key inside
-- the release blob and so cannot be indexed at a fixed path; this derived table
-- makes "releases/locations for a CID" a proper indexed query. Rebuilt for a
-- release whenever that release is (re)cached.
create table if not exists "locations" (
  -- Repository ID (URN).
  "repo"    text not null,
  -- Artifact CID.
  "cid"     text not null,
  -- Release (Object) ID the artifact/location belongs to.
  "release" text not null,
  -- Contributor DID; '' for an artifact that has no locations yet.
  "did"     text not null,
  -- Location URL; '' for an artifact that has no locations yet.
  "url"     text not null,
  -- The primary key's implicit index leads with (repo, cid), so it already
  -- serves our only lookups -- "WHERE repo = ? AND cid = ?" -- as a covering
  -- index (release/did/url are in the key too). No separate index is needed;
  -- one on (repo, cid) is redundant and the query planner ignores it.
  primary key ("repo", "cid", "release", "did", "url")
) strict;
