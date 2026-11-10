# From the OpenStreetMap minutely replication

- **Source:** <https://planet.openstreetmap.org/replication/minute/>, sequences 7363600 (2026-11-08 08:00:21 UTC)
  and 7363630 (2026-11-08 08:30:41 UTC), downloaded on 2026-11-08.
- **License:** © OpenStreetMap contributors, under the [Open Database License](https://opendatacommons.org/licenses/odbl/)
  1.0, in `LICENSE`.

| File | What it holds |
|---|---|
| `minute/007/363/600.osc.gz` | One minute of edits worldwide, 36 kB: `tests/osc.rs` checks its changes against a plain count of its elements in each section, and a few objects in full, and the osmChange benchmark parses it |
| `minute/007/363/600.state.txt` | Its state, which `tests/replication.rs` parses |
| `minute/007/363/630.osc.gz` | One minute of edits worldwide, written by `osmdbt-create-diff/0.9`: 326 nodes, 97 ways and 3 relations in `create`, `modify` and `delete` sections, with full metadata; deleted objects have no tags, and deleted nodes keep their last location |
| `minute/007/363/630.state.txt` | Its state: a `#` comment with the date it was written, `sequenceNumber` and `timestamp` (with escaped colons) |
