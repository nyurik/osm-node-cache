# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.0](https://github.com/nyurik/osmflow/compare/osmflow-sync-v0.13.1...osmflow-sync-v0.14.0) - 2026-11-12

### Changed

- [**breaking**] Geometries are stored as TWKB by default: `osm.nodes.geom` and `osm.ways.geom` are TWKB with OSM's
  7 decimal digits in a `bytea`, about a quarter of a line's WKB, which PostGIS reads back exactly as the locations
  were. `ImportOpts::geometry` (`GeometryFormat::Wkb`) keeps PostGIS's `geometry` instead. The choice is made at the
  import, recorded in `osm.state.geometry` and `DbState::geometry`, and followed by updates and rebuilds. SQL reads
  either format with `osm.geom(geom)`, which Postgres inlines, so an index on `osm.geom(geom)` serves queries and
  views ([#57](https://github.com/nyurik/osmflow/pull/57))
- [**breaking**] The `osm` schema has format 2, which 0.13 databases do not have (0.14 stops with `column "geometry"
  does not exist`). Import again, or upgrade in place: 0.13 stored geometries as 0.14's `wkb` format does. With
  PostGIS in schema `public`:

  ```sql
  BEGIN;
  ALTER TABLE osm.state ADD COLUMN geometry text NOT NULL DEFAULT 'wkb';
  ALTER TABLE osm.state ALTER COLUMN geometry DROP DEFAULT;
  UPDATE osm.state SET version = 2;
  CREATE FUNCTION osm.geom(g public.geometry) RETURNS public.geometry LANGUAGE sql IMMUTABLE PARALLEL SAFE
      AS $$ SELECT g $$;
  COMMIT;
  ```

- A rebuild from the database fails, naming the way or the node, when its stored geometry does not have one point for
  each of its nodes, instead of giving nodes the wrong locations.

## [0.13.1](https://github.com/nyurik/osmflow/compare/osmflow-sync-v0.13.0...osmflow-sync-v0.13.1) - 2026-11-11

### Added

- `Mirror::open` gives an `OpenMirror`: the database connection and the reverse index, and after its first update
  the node file, held until it is dropped ([#56](https://github.com/nyurik/osmflow/pull/56)). `OpenMirror::update` borrows it mutably and
  `OpenMirror::ways_through` shares it, so a program can look up ways between batches without opening the index
  again, and no lookup can run while a batch writes:

  ```rust
  use osmflow::osc::Replication;
  use osmflow_sync::{Mirror, NodeWay, UpdateOpts};

  fn follow() -> Result<(), Box<dyn std::error::Error>> {
      let mirror = Mirror::new("postgresql://osm@localhost/osm", "/data/osm");
      let replication = Replication::local("/data/diffs");
      let mut open = mirror.open()?;
      loop {
          // &mut: writes the database, the node file and the index
          open.update(&replication, &UpdateOpts { max_diffs: 1, ..UpdateOpts::default() })?;
          // &: the ways through the nodes an edit touched, from the same open index
          for NodeWay { node, way } in open.ways_through(&[1_886_651_627, 13_000_000_000])? {
              println!("way {way} passes through node {node}");
          }
      }
  }
  ```

- `OpenMirror::ways_through` gives the ways through some nodes, as `NodeWay { node, way }` sorted by node and way: the
  lookup an update makes to find the ways a moved node changes. The nodes may be in any order and repeat; negative
  IDs, which only editors' files have, find none. After a crash between the database's commit and the index's it may
  list a way that no longer has the node, so check the ways against `osm.ways`; after a failed update it may miss
  ways until the next update succeeds.

### Changed

- `Mirror::update` is `Mirror::open`, then `OpenMirror::update`, and applies the diffs as before ([#56](https://github.com/nyurik/osmflow/pull/56)).
- `open` checks the reverse index against the database and brings it up to date: an index that is missing, of another
  database or ahead of it is an error, which `Mirror::rebuild` fixes. A mirror kept only for lookups never opens the
  node file.
- Every update checks both files against the database, so a database restored or replaced under a held mirror is an
  error, and brings them up to date if a crash or an earlier error left them behind.
- A batch that fails leaves the node file closed, as it can hold locations the database did not commit; the next
  update opens it again.
- The temporary tables a batch copies its rows into are made once for each open mirror, not once for each update.

### Other

- The README has status badges: crates.io, docs.rs, license, CI, and this crate's coverage from Codecov ([#53](https://github.com/nyurik/osmflow/pull/53)).

## [0.13.0](https://github.com/nyurik/osmflow/tree/osmflow-sync-v0.13.0) - 2026-11-10

The first release of `osmflow-sync`: OpenStreetMap data in PostgreSQL with PostGIS, kept current from replication
diffs ([#51](https://github.com/nyurik/osmflow/pull/51)).

### Added

- `Mirror::import` loads a PBF file, a planet or an extract, into the `osm` schema: tagged nodes, every way with its
  line geometry, and every relation, with binary `COPY` on several connections. Beside the database it makes the node
  file, a `PagedStore`, and the reverse index from each node to the ways through it, in `redb`.
- The import takes an extract's replication sequence from its header, also when the stream is a local copy of it,
  and finds a planet file's from its timestamp. A given sequence that would skip changes is an error; an earlier one
  is a warning.
- `Mirror::update` applies the diffs after the database's sequence, up to `max_diffs` of them in one transaction,
  once it has checked that the stream is the database's. A moved node rewrites the geometry of the ways through it and
  adds 1 to their `minor`.
- Every row has the revision that last wrote it, `osm.changes` logs each change after the import and `osm.batches`
  each revision, so tables made from these can be kept current too.
- After a crash between the database's commit and the files', the node file and the reverse index catch up on the
  next update.
- `Mirror::rebuild` makes both files again without changing the database, with node locations from the database, or
  from a PBF file and the diffs after it.
- `Mirror::state` and `Mirror::status` give the database's sequence and revision, and the files'.
- The reverse index's (node, way) pairs are sorted on disk, in runs of 1 GiB sorted on all cores, so a planet's do not
  need to fit in memory.
