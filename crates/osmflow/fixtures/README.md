# Test fixtures

OSM PBF files from other projects, each in its own directory with a README saying where it came from, under which
license, and what it tests. `generated/` holds files osmflow's own tests write, for cases nobody publishes.
`osc/` holds change files (osmChange and replication state files), laid out the same way.

Every `*.pbf` file here is read by `tests/fixtures.rs`, which checks that osmflow reads it the same way in every mode
(one thread, parallel in order, parallel unordered), that the `osmpbf` and `osmpbfreader` crates read the same objects
from it, and that the result matches the snapshot in `tests/snapshots/`.

Files with OpenStreetMap data are © OpenStreetMap contributors, available under the
[Open Database License](https://opendatacommons.org/licenses/odbl/).
