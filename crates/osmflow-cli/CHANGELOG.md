# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.1](https://github.com/nyurik/osmflow/compare/osmflow-cli-v0.14.0...osmflow-cli-v0.14.1) - 2026-11-12

### Added

- *(geometry)* [**breaking**] assemble the areas of multipolygon relations ([#62](https://github.com/nyurik/osmflow/pull/62))
- *(pbf)* read a file range by range on the caller's threads ([#61](https://github.com/nyurik/osmflow/pull/61))

### Other

- osmflow-cli output snapshots and coverage ([#59](https://github.com/nyurik/osmflow/pull/59))

## [0.14.0](https://github.com/nyurik/osmflow/compare/osmflow-cli-v0.13.1...osmflow-cli-v0.14.0) - 2026-11-12

### Added

- `osmflow sync import --geometry twkb|wkb` chooses how the mirror stores geometries: TWKB by default, a quarter of
  WKB's size, or PostGIS's `geometry`. SQL reads either with `osm.geom(geom)`
  ([#57](https://github.com/nyurik/osmflow/pull/57))

### Changed

- [**breaking**] A mirror imported by 0.13 needs a new import, or the upgrade in osmflow-sync 0.14.0's changelog,
  before `osmflow sync` can update it.

### Fixed

- On Windows, the files of a replication stream in a local directory are numbered right: their paths are split at
  `\` too.

## [0.13.1](https://github.com/nyurik/osmflow/compare/osmflow-cli-v0.13.0...osmflow-cli-v0.13.1) - 2026-11-11

### Changed

- `osmflow sync update` goes through `osmflow-sync`'s new `Mirror::open`, and applies the diffs as before; it checks
  the reverse index against the database before it reads the stream ([#56](https://github.com/nyurik/osmflow/pull/56)).
- The release binaries are built and packed by `just dist`, on every CI run, and attached to the release by the
  release-plz job: the same `osmflow-<target>.tar.gz`, or `.zip` on Windows, each with the binary and both licenses,
  as `cargo binstall osmflow-cli` expects. The Windows binaries link the C runtime statically, as before, so they
  need no Visual C++ redistributable. The release has no `.sha256` files any more: GitHub shows each file's SHA-256
  digest ([#53](https://github.com/nyurik/osmflow/pull/53)).

### Other

- Building the whole workspace, as the coverage run does, no longer fails on osmpbf's two zlib backends
  ([#54](https://github.com/nyurik/osmflow/pull/54)).
- The README has status badges: crates.io, docs.rs, license, CI, and this crate's coverage from Codecov ([#53](https://github.com/nyurik/osmflow/pull/53)).

## [0.13.0](https://github.com/nyurik/osmflow/compare/osmflow-cli-v0.12.0...osmflow-cli-v0.13.0) - 2026-11-10

### Added

- `osmflow sync import`, `update`, `rebuild` and `status` keep OpenStreetMap data in PostgreSQL with PostGIS, current
  from replication diffs, through the new `osmflow-sync` crate ([#51](https://github.com/nyurik/osmflow/pull/51)).
  The database and the directory of its files come from `--db` and `--dir`, or from `DATABASE_URL` and
  `OSMFLOW_SYNC_DIR`. The command is in the default `sync` feature.
- An error's causes are each printed once.

### Other

- The node stores share their header code; the files they write are unchanged
  ([#52](https://github.com/nyurik/osmflow/pull/52))

## [0.12.0](https://github.com/nyurik/osmflow/compare/osmflow-cli-v0.11.0...osmflow-cli-v0.12.0) - 2026-11-10

### Added

- [**breaking**] osmChange support, sequence search, catch-up plans, `osmflow seq` ([#48](https://github.com/nyurik/osmflow/pull/48))

### Other

- changelog per crate, CLI tool is the only one on GitHub release page ([#47](https://github.com/nyurik/osmflow/pull/47))

## [0.11.0](https://github.com/nyurik/osmflow/releases/tag/osmflow-cli-v0.11.0) - 2026-11-09

The first release of the `osmflow` command.

### Added

- `osmflow dump` writes a PBF file's objects as text, GeoJSON or GeoJSON lines, chosen by ID, type or tag, with way
  geometries.
- `osmflow stats` summarizes a file: its header, blocks, counts, ID ranges, extent and order.
