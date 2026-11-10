# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.12.1](https://github.com/nyurik/osmflow/compare/osmflow-cli-v0.12.0...osmflow-cli-v0.12.1) - 2026-11-10

### Added

- add osmflow-sync -- OSM data to PostgreSQL repl ([#51](https://github.com/nyurik/osmflow/pull/51))

### Other

- reduce dups ([#52](https://github.com/nyurik/osmflow/pull/52))

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
