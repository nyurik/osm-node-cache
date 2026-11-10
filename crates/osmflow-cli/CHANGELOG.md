# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
