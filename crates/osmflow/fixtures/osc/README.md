# osmChange fixtures

osmChange and replication state files, read by `tests/osc.rs`, `tests/replication.rs` and the osmChange benchmark. The
cases written for one error or feature each are inline in those tests; the replication streams they read are written
by the tests themselves. Each subdirectory named after a project or server holds files from elsewhere, with a README
saying where they came from, under which license, and what they hold; `tests/osc.rs` reads every one of them.

## From other projects and servers

OsmChange files (`.osc`, `.osc.gz`) and replication state files from other projects and from the OpenStreetMap and
Geofabrik replication servers, in `baremaps/`, `geofabrik/`, `go-osm/`, `imposm-changes/`, `inputosm/`, `osm-testdata/`,
`osmosis/`, `paulmach-osm/` and `planet/`. Replication files keep the server's `AAA/BBB/CCC` path layout.
