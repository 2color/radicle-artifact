-- Sort releases by creation time in SQL.

-- Creation time in milliseconds, read from the release blob. A virtual column
-- needs no backfill: existing rows get a value at once.
alter table "releases" add column "timestamp" integer
  generated always as (json_extract("release", '$.timestamp')) virtual;

-- Serves "WHERE repo = ? ORDER BY timestamp DESC, id DESC" without a sort step.
create index if not exists "ix_releases_repo_timestamp"
  on "releases" (repo, "timestamp" desc, id desc);
