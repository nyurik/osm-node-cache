# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
