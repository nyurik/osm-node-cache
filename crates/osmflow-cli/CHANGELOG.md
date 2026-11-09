# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.0](https://github.com/nyurik/osmflow/releases/tag/osmflow-cli-v0.11.0) - 2026-11-09

The first release of the `osmflow` command.

### Added

- `osmflow dump` writes a PBF file's objects as text, GeoJSON or GeoJSON lines, chosen by ID, type or tag, with way
  geometries.
- `osmflow stats` summarizes a file: its header, blocks, counts, ID ranges, extent and order.
