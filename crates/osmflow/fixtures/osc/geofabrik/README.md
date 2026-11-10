# From Geofabrik's daily replication

- **Source:** <https://download.geofabrik.de/>, the daily updates of two extracts, downloaded on 2026-11-08:
  `europe/monaco-updates/` sequence 4960 and `australia-oceania/niue-updates/` sequence 2410.
- **License:** © OpenStreetMap contributors, under the [Open Database License](https://opendatacommons.org/licenses/odbl/)
  1.0, in `LICENSE`.

| File | What it holds |
|---|---|
| `monaco/000/004/960.osc.gz` | One day of edits in Monaco, written by osmium 1.16, with versions and timestamps but no changesets or users, as in Geofabrik's public downloads |
| `niue/000/002/410.osc.gz` | A day without edits in Niue: an `osmChange` element with nothing in it |
| `*/…/*.state.txt` | Their states, whose `#` comment gives the minutely sequence number the day ends at |

`../../planetiler/` and `../../osm-io/` hold extracts of the same two regions.
