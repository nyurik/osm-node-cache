# From Osmosis

- **Source:** <https://github.com/openstreetmap/osmosis>, `osmosis-xml/src/test/resources/data/template/v0_6/` and
  `osmosis-set/src/test/resources/data/template/v0_6/`, commit `31c6dcf7f2c744906a509716c80f77f1c0f8f52c`
  (2024-12-29).
- **License:** public domain, as `osmosis/src/dist/copying.txt` says. The data is made up.

These are templates: Osmosis's tests replace `%VERSION%` in the `generator` attribute.

| File | What it holds |
|---|---|
| `xml-task-tests-v0_6.osc` | Every object type in `create`, `modify` and `delete`, with negative coordinates, an empty user and objects without a user |
| `xml-delete-no-coordinates.osc` | A deleted node without a location |
| `empty-change.osc` | An `osmChange` element with nothing in it |
