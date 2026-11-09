use std::path::PathBuf;
use std::{fmt, io, result};

/// The result of every fallible call of [`osc`](crate::osc).
pub(crate) type Result<T, E = OscError> = result::Result<T, E>;

/// An error reading an osmChange file or a replication source.
#[derive(Debug, thiserror::Error)]
pub enum OscError {
    /// Reading the input failed, for a reader given a stream rather than a file. A file's errors are
    /// [`File`](Self::File), which names it.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// Opening or reading a file failed: an osmChange file, or a file of a replication directory.
    #[error("{}: {source}", path.display())]
    File {
        /// The file.
        path: PathBuf,
        /// What the operating system reported.
        source: io::Error,
    },
    /// The input is not well-formed XML, or not an osmChange document.
    #[error("{position}: {kind}")]
    Xml {
        /// Where the error is.
        position: XmlPosition,
        /// What is wrong.
        kind: XmlError,
    },
    /// A replication state file does not parse.
    #[error("{origin}: invalid state file: {kind}")]
    State {
        /// The state file's path or URL.
        origin: String,
        /// What is wrong.
        kind: StateError,
    },
    /// Downloading a file from a replication server failed, after any retries its [`Fetch`](crate::osc::Fetch)
    /// makes.
    #[error("{url}: {source}")]
    Fetch {
        /// The file's URL.
        url: String,
        /// The network or HTTP error.
        source: io::Error,
    },
    /// The server's state lists a diff the server does not have, as a mirror's can while it catches up.
    #[error("{url} is not on the server, though the server's state says it is published")]
    NotFound {
        /// The diff's URL.
        url: String,
    },
    /// A read asked for a diff past the stream's latest one.
    #[error("sequence {sequence} is past the source's latest state, {latest}")]
    NotPublished {
        /// The diff asked for.
        sequence: u64,
        /// The latest diff the stream's state lists.
        latest: u64,
    },
    /// The thread decoding diffs ahead ended without a result, as when it panicked.
    #[error("the thread reading diffs ahead stopped unexpectedly")]
    ReadAhead,
}

/// Where in an osmChange file an error was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlPosition {
    /// The file, if the reader was opened from one.
    pub file: Option<PathBuf>,
    /// The byte offset of the markup the error is in, from the start of the XML (after decompression).
    pub offset: u64,
    /// The name of the element the error is in, if it got that far.
    pub element: Option<String>,
}

impl fmt::Display for XmlPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}: ", file.display())?;
        }
        write!(f, "byte {}", self.offset)?;
        if let Some(element) = &self.element {
            write!(f, ", <{element}>")?;
        }
        Ok(())
    }
}

/// What is wrong with an osmChange file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum XmlError {
    /// The input ends before `</osmChange>`, as a cut download does.
    #[error("the file ends before the end of the osmChange element")]
    Truncated,
    /// The markup breaks XML's syntax. The string says how.
    #[error("malformed XML: {0}")]
    Syntax(&'static str),
    /// A DTD, entity declaration or CDATA section. osmChange files have none, and refusing them rules out entity
    /// expansion attacks.
    #[error("DTDs, entity declarations and CDATA sections are not allowed")]
    Declaration,
    /// Text other than white space between elements, which osmChange has no place for.
    #[error("text is not allowed between elements")]
    Text,
    /// An element where osmChange does not allow it. The string says where.
    #[error("the element is not allowed {0}")]
    Unexpected(&'static str),
    /// An end tag that does not close the open element, which the string names.
    #[error("the end tag does not match the open element <{0}>")]
    Mismatched(&'static str),
    /// An element lacks an attribute it must have, such as a node's `id`.
    #[error("the required attribute {0:?} is missing")]
    MissingAttribute(&'static str),
    /// An element has an attribute twice, which XML does not allow.
    #[error("the attribute {0:?} is repeated")]
    DuplicateAttribute(&'static str),
    /// An attribute value does not parse as its type: an integer in range, a coordinate whose 1e-7 degrees fit an
    /// `i32` (±214.7°), a UTC timestamp, a member type or a boolean.
    #[error("invalid {attribute} {value:?}")]
    Value {
        /// The attribute's name.
        attribute: &'static str,
        /// Its value, as the file gives it.
        value: String,
    },
    /// An attribute value has a `<`, a control character, or a reference to an undefined entity or an invalid
    /// character.
    #[error("an attribute value has an invalid character or reference")]
    Escape,
    /// An attribute value is not UTF-8, which osmChange files must be.
    #[error("an attribute value is not valid UTF-8")]
    Utf8,
    /// A created or modified node has no location; only deleted nodes may lack one.
    #[error("a node outside a delete section has no location")]
    NoLocation,
    /// A node has a latitude without a longitude, or the other way round.
    #[error("a node has only one of lat and lon")]
    HalfLocation,
    /// An element over 16 MiB, or an object's text over 4 GiB: limits that keep a damaged file from taking all
    /// memory.
    #[error("an element is larger than 16 MiB, or an object's text larger than 4 GiB")]
    TooLarge,
}

/// What is wrong with a replication state file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    /// A required key, `sequenceNumber` or `timestamp`, is missing.
    #[error("{0} is missing")]
    Missing(&'static str),
    /// A value does not parse: the sequence number as an unsigned integer, or the timestamp as UTC ending in `Z`.
    #[error("invalid {key} {value:?}")]
    Value {
        /// The key.
        key: &'static str,
        /// Its value, as the file gives it.
        value: String,
    },
}
