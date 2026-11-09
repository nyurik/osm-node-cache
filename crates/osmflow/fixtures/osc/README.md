# osmChange fixtures

osmChange and replication state files, read by `tests/osc.rs` and `tests/replication.rs`. The cases written for one
error or feature each are inline in those tests; the replication streams they read are written by the tests themselves.
Each subdirectory named after a project or server holds files from elsewhere, with a README saying where they came from,
under which license, and what they hold; `tests/osc.rs` reads every one of them.

## Real data

| File | What it is |
|---|---|
| `planet-minute-7363600.osc.gz` | The planet's minute diff 7363600 (2026-11-08 08:00 UTC), as published at `https://planet.openstreetmap.org/replication/minute/007/363/600.osc.gz`. Its changes are checked against a plain count of its elements in each section, and a few objects in full. |
| `planet-minute-7363600.state.txt` | That diff's state file. |

The planet's data is © OpenStreetMap contributors, available under the
[Open Database License](https://opendatacommons.org/licenses/odbl/).

## From other projects and servers

OsmChange files (`.osc`, `.osc.gz`) and replication state files from other projects and from the OpenStreetMap and
Geofabrik replication servers, in `baremaps/`, `geofabrik/`, `go-osm/`, `imposm-changes/`, `inputosm/`, `osm-testdata/`,
`osmosis/`, `paulmach-osm/` and `planet/`. Replication files keep the server's `AAA/BBB/CCC` path layout.
