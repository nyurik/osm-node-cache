# osmChange files and replication

How osmflow reads osmChange files, and the replication streams that publish them as numbered diffs. This is the `osc`
feature, in `osmflow::osc`; downloading from a server also needs the `http` feature, or a `Fetch` of the caller's. The
[README](../README.md#changes) has an example.

## osmChange files

An osmChange file is XML: an `osmChange` element holding `create`, `modify` and `delete` sections, which hold nodes,
ways and relations, each a whole version of the object. Replication diffs are osmChange files, compressed with gzip.

* **`OscReader` streams the changes of one file in file order.** Each `Change` is an `Action`, the section it is in,
  and an owned `Object`: a node with its location, a way with its node IDs, or a relation with its members, each with
  its tags and metadata.
* **Every change is kept.** A diff can hold several versions of one object, and sections can come back (`modify`,
  `delete`, `modify`); the reader gives each one in its place, and merges nothing.
* **The input** is a file, decompressed if it starts with gzip's magic bytes, whatever its name, with every member of a
  multi-member gzip file read; or any `Read` of uncompressed XML. The reader buffers its input itself, 256 KiB at a
  time.
* **Metadata fields are each optional,** as files leave different ones out: Geofabrik's public diffs have no changeset,
  user ID or user name. A user name is shared, as one `Arc<str>`, between consecutive objects of the same user.
* **A deleted object** may have only its ID and version. A deleted node keeps its location when the file gives it, as
  the planet's diffs do; created and modified nodes must have one.
* **An object's strings** (its tags, and a relation's roles) are kept in one string, cut by their ends, so an object
  takes a few allocations however many tags it has.

### Parsing

The reader is osmflow's own: a tokenizer over its buffer, using `memchr` to find markup, and the format's structure on
top. It reads the subset of XML osmChange files use, and checks it strictly.

* **Attributes are matched as bytes,** and their values read straight from the bytes: integers, and timestamps in the
  fixed form `YYYY-MM-DDThh:mm:ssZ`, checked against the calendar. A value is unescaped only when it holds `&`, `<` or
  a control character.
* **Coordinates are fixed-point decimals,** never parsed as floating-point numbers. Digits past the seventh decimal
  round half away from zero, as PBF writers round, and a latitude must be within ±90° and a longitude within ±180°. PBF files store
  locations to 1e-7 degrees, so a location from a diff equals the same location from a PBF file.
* **The structure is checked:** the root is `osmChange`, it holds only the three sections, they hold only nodes, ways
  and relations, and those hold only their tags, way nodes and members. Unknown attributes are ignored. Comments and
  processing instructions are skipped, as is a byte order mark.
* **The section decides the action.** A `visible` attribute must be a boolean, but is not compared with the section,
  as writers disagree on its value in a `delete` section.
* **DTDs, entity declarations and CDATA sections are rejected.** osmChange files have none, and refusing them rules out
  entity expansion attacks.
* **Limits** keep a damaged file from taking all memory: one element's markup takes at most 16 MiB, and an object holds
  at most a million node refs, members or tags, and 64 MiB of tag or role text, far above any real object.
* **Errors say where:** an `OscError::Xml` has an `XmlError` saying what is wrong, and an `XmlPosition` with the file,
  the byte offset in the decompressed XML, and the element. After an error, the reader ends.

## Replication streams

A replication server publishes a stream of numbered diffs: diff 7363600 is `007/363/600.osc.gz`, with its state file
`007/363/600.state.txt`, and `state.txt` holds the latest state. `sequence_path` and `path_sequence` convert between
the two forms.

* **`State`** is a state file's sequence number and timestamp. State files are Java properties: comment lines are
  skipped, backslash escapes undone (the timestamp's colons are written `\:`), and both fields are required.
* **`Replication`** reads a stream from a directory laid out as the servers lay theirs out:
  * `Replication::local` reads a directory, and downloads nothing;
  * `Replication::remote` reads a server through a `Fetch`, with a directory as its cache;
  * `Replication::http` does the same over HTTP, with the `http` feature.
* **`Replication::diffs`** gives the diffs from a sequence up to the latest state, in order, each a `Diff` with its own
  state and all its changes. One background thread downloads the next diffs, one file at a time, and each diff is
  decoded on a thread of its own as soon as it is in the directory. The iterator holds up to
  `Replication::with_read_ahead` diffs ahead of the one being read, by default as many as the cores, from 2 to 4. Each
  is held whole in memory until it is read, so a stream of large diffs needs a small read-ahead. After an error, the
  iterator ends.
* **`state`, `sequence_state`, `diff_file` and `diff`** read one file each, downloading it first if needed.

### The cache

A remote stream's directory is its cache, and once filled it can be read with `Replication::local`.

* **Every file is downloaded once.** A file already in the directory is never fetched again. Only `state.txt` is
  fetched anew, each time the latest state is asked for.
* **Every file is checked before it is kept:** a state file must parse and name the sequence it was fetched for, and a
  diff must be complete gzip, every member of it decompressed with its trailer checked. A file that fails its check is
  fetched again, up to three tries in all.
* **Files are written whole or not at all:** to a temporary file beside the final one, synced, then renamed into place.
  So no file in the cache is ever cut short, even after a crash.
* **Nothing past the server's latest state is fetched,** even if the server already has it: a read past it is an
  `OscError::NotPublished`. A diff the server's state lists but the server does not have, as a mirror's can while it
  catches up, is an `OscError::NotFound`.

### Fetching

* **`Fetch`** is the one thing a server needs: the body of a URL, or `None` for a file the server does not have. The
  replication code depends on no HTTP crate, and a program can implement `Fetch` with the client it already uses.
* **`HttpFetch`** implements it with `ureq`, which is blocking and has a small dependency tree. It makes one request at
  a time, follows redirects, and names itself `osmflow/<version>` in the `User-Agent`. It retries a request answered
  with 408, 429 or a 5xx status, or failing to connect or to read, up to 8 times by default: after the pause the
  server's `Retry-After` asks for (at most 10 minutes), or else after 1, 2, 4 and up to 60 seconds, each with a small
  random addition so clients do not retry in step.
