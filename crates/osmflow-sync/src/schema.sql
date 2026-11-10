-- The `osm` schema, created by an import. Primary keys are added once the data is in.
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE SCHEMA osm;

-- One row: where the database is in its replication stream.
CREATE TABLE osm.state (
    -- A random number shared with the node file and the reverse index, which must match it
    id        int8        NOT NULL,
    -- The format of this schema
    version   int4        NOT NULL,
    -- The last batch applied; the import is revision 1
    rev       int8        NOT NULL,
    -- The replication stream's base URL, and the last sequence applied from it
    url       text,
    sequence  int8        NOT NULL,
    timestamp timestamptz
);

-- One row per revision: the import, then each update batch.
CREATE TABLE osm.batches (
    rev            int8 PRIMARY KEY,
    first_sequence int8        NOT NULL,
    last_sequence  int8        NOT NULL,
    timestamp      timestamptz,
    applied_at     timestamptz NOT NULL DEFAULT now(),
    nodes          int8        NOT NULL,
    ways           int8        NOT NULL,
    relations      int8        NOT NULL
);

-- Tagged nodes. Untagged nodes are only in the node file.
CREATE TABLE osm.nodes (
    id        int8  NOT NULL,
    version   int4,
    changeset int8,
    ts        timestamptz,
    rev       int8  NOT NULL,
    tags      jsonb NOT NULL,
    geom      geometry(Point, 4326)
) WITH (autovacuum_enabled = off);

-- Every way. `minor` counts the changes of its geometry since its version: a node of it moved or was deleted.
CREATE TABLE osm.ways (
    id        int8   NOT NULL,
    version   int4,
    changeset int8,
    ts        timestamptz,
    minor     int4   NOT NULL,
    rev       int8   NOT NULL,
    tags      jsonb  NOT NULL,
    nodes     int8[] NOT NULL,
    -- One point per node, or NULL when a node is missing or the way has fewer than two
    geom      geometry(LineString, 4326)
) WITH (autovacuum_enabled = off);

-- Every relation, with its members as three arrays of the same length.
CREATE TABLE osm.relations (
    id           int8     NOT NULL,
    version      int4,
    changeset    int8,
    ts           timestamptz,
    rev          int8     NOT NULL,
    tags         jsonb    NOT NULL,
    -- n, w or r
    member_types "char"[] NOT NULL,
    member_ids   int8[]   NOT NULL,
    member_roles text[]   NOT NULL
) WITH (autovacuum_enabled = off);

-- Every row change of the three tables after the import.
CREATE TABLE osm.changes (
    rev    int8   NOT NULL,
    -- n, w or r
    kind   "char" NOT NULL,
    id     int8   NOT NULL,
    -- u: written (created or changed), d: deleted, g: a way's geometry changed by its nodes
    action "char" NOT NULL
);
CREATE INDEX ON osm.changes (rev);
