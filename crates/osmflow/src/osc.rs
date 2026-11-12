//! osmChange files (`.osc`, `.osc.gz`) and the replication streams that publish them.
//!
//! [`OscReader`] streams the changes of one file in file order, each an [`Action`] and an owned [`Object`] with all
//! the metadata the file has. [`Replication`] reads a replication stream, from a local directory laid out as the
//! servers lay theirs out or (with the `http` feature) from a server through a local cache, and decodes the next
//! diffs ahead on background threads. [`locate`] finds the diff a time falls in, and [`plan`] lays out a catch-up over
//! day, hour and minute streams, both from state files read through any [`StateSource`].
//!
//! ```
//! use osmflow::osc::{Action, Object, OscError, OscReader};
//!
//! fn summarize(path: &str) -> Result<(), OscError> {
//!     let mut deleted_nodes = 0;
//!     for change in OscReader::open(path)? {
//!         let change = change?;
//!         match &change.object {
//!             Object::Node(node) if change.action == Action::Delete => {
//!                 deleted_nodes += 1;
//!                 // A deleted node keeps its last location when the file gives it.
//!                 if let Some(location) = node.location {
//!                     let (lat, lon) = (location.lat, location.lon);
//!                 }
//!             }
//!             Object::Way(way) => {
//!                 let (id, node_ids, version) = (way.id, &way.refs, way.meta.version);
//!                 for (key, value) in way.tags.iter() { /* handle the tags */ }
//!             }
//!             Object::Relation(relation) => {
//!                 for member in relation.members() {
//!                     let (kind, id, role) = (member.kind, member.id, member.role);
//!                 }
//!             }
//!             Object::Node(_) => {}
//!         }
//!     }
//!     println!("{deleted_nodes} nodes deleted");
//!     Ok(())
//! }
//! ```

mod error;
#[cfg(feature = "http")]
mod http;
mod parse;
mod plan;
mod replication;
mod value;

use std::sync::Arc;

pub use self::error::{OscError, StateError, XmlError, XmlPosition};
#[cfg(feature = "http")]
pub use self::http::HttpFetch;
pub use self::parse::OscReader;
pub use self::plan::{COMMIT_DELAY, Located, PARALLEL, Start, StateSource, Step, locate, plan};
pub use self::replication::{
    Diff, Diffs, Fetch, Replication, ServerStates, State, path_sequence, sequence_path,
};
use crate::Timestamp;
use crate::model::{Location, Member, MemberType};

/// What a change does to its object: the section of the file it is in. It displays, parses and converts to a
/// `&'static str` as the section element's name: `create`, `modify` or `delete`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::Display,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::VariantNames,
)]
#[strum(serialize_all = "lowercase")]
pub enum Action {
    /// The object is new, usually at version 1.
    Create,
    /// The object's new version, whole: tags, nodes or members it does not list were removed.
    Modify,
    /// The object was deleted. The file may give only its ID and version.
    Delete,
}

/// One change of an osmChange file: an object's new version, and what was done to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The section of the file the change is in.
    pub action: Action,
    /// The object after the change; after a deletion, what the file gives of its last version.
    pub object: Object,
}

/// A node, way or relation, as an osmChange file gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Object {
    /// A point, with its location.
    Node(Node),
    /// A line or area, as its nodes' IDs in order.
    Way(Way),
    /// A group of objects, each with a role.
    Relation(Relation),
}

impl Object {
    /// The object's type, as a relation member names it.
    #[must_use]
    pub fn kind(&self) -> MemberType {
        match self {
            Self::Node(_) => MemberType::Node,
            Self::Way(_) => MemberType::Way,
            Self::Relation(_) => MemberType::Relation,
        }
    }

    /// The object's ID, unique only within its type: a node and a way may have the same ID.
    #[must_use]
    pub fn id(&self) -> i64 {
        match self {
            Self::Node(o) => o.id,
            Self::Way(o) => o.id,
            Self::Relation(o) => o.id,
        }
    }

    /// The object's metadata, whichever type it is.
    #[must_use]
    pub fn meta(&self) -> &Meta {
        match self {
            Self::Node(o) => &o.meta,
            Self::Way(o) => &o.meta,
            Self::Relation(o) => &o.meta,
        }
    }

    /// The object's tags, whichever type it is.
    #[must_use]
    pub fn tags(&self) -> &Tags {
        match self {
            Self::Node(o) => &o.tags,
            Self::Way(o) => &o.tags,
            Self::Relation(o) => &o.tags,
        }
    }
}

/// An object's metadata. Each field is `None` when the file leaves it out: Geofabrik's public diffs have no changeset,
/// user ID or user name, for one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// The version this change makes: each edit of an object adds 1.
    pub version: Option<i32>,
    /// When this version was made.
    pub timestamp: Option<Timestamp>,
    /// The changeset the edit was uploaded in.
    pub changeset: Option<i64>,
    /// The ID of the user who made the edit, which stays the same when they change their name.
    pub uid: Option<i32>,
    /// The user name. A reader shares one string between consecutive objects of the same user.
    pub user: Option<Arc<str>>,
}

/// A node of an osmChange file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Unique among nodes; ways and relations have IDs of their own.
    pub id: i64,
    /// What the file gives of the version, timestamp, changeset and user.
    pub meta: Meta,
    /// In file order; a deletion may leave them out.
    pub tags: Tags,
    /// Always present in created and modified nodes; present in deleted ones when the file gives it, as the planet's
    /// diffs do.
    pub location: Option<Location>,
}

/// A way of an osmChange file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Way {
    /// Unique among ways; nodes and relations have IDs of their own.
    pub id: i64,
    /// What the file gives of the version, timestamp, changeset and user.
    pub meta: Meta,
    /// In file order; a deletion may leave them out.
    pub tags: Tags,
    /// The IDs of the way's nodes; usually empty in a deleted way.
    pub refs: Vec<i64>,
}

/// A relation of an osmChange file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// Unique among relations; nodes and ways have IDs of their own.
    pub id: i64,
    /// What the file gives of the version, timestamp, changeset and user.
    pub meta: Meta,
    /// In file order; a deletion may leave them out.
    pub tags: Tags,
    members: Box<[(MemberType, i64)]>,
    roles: Strings,
}

impl Relation {
    /// The relation's members, in order; usually none in a deleted relation.
    pub fn members(&self) -> impl ExactSizeIterator<Item = Member<'_>> + Clone {
        self.members
            .iter()
            .enumerate()
            .map(|(i, &(kind, id))| Member {
                kind,
                id,
                role: self.roles.get(i),
            })
    }
}

/// An object's tags, in file order, stored in one string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags(Strings);

impl Tags {
    /// The number of tags.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.ends.len() / 2
    }

    /// Whether the object has no tags.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.ends.is_empty()
    }

    /// The (key, value) pairs, in file order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &str)> + Clone {
        (0..self.len()).map(|i| (self.0.get(2 * i), self.0.get(2 * i + 1)))
    }

    /// The value of the first tag with this key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.iter().find(|&(k, _)| k == key).map(|(_, v)| v)
    }
}

/// Strings stored one after another in one allocation, cut by their ends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Strings {
    text: Box<str>,
    ends: Box<[u32]>,
}

impl Strings {
    fn get(&self, i: usize) -> &str {
        let start = i.checked_sub(1).map_or(0, |prev| self.ends[prev]);
        &self.text[start as usize..self.ends[i] as usize]
    }
}

/// The growing buffers a [`Strings`] is copied from, reused from object to object.
#[derive(Debug, Default)]
struct StringsBuf {
    text: String,
    ends: Vec<u32>,
}

/// The most strings of one kind an object may have, and their bytes: a million tags or member roles, and 64 MiB of
/// them, far above any real object, bound the memory a damaged file can take.
const MAX_STRINGS: usize = 2 << 20;
const MAX_TEXT: usize = 64 << 20;

impl StringsBuf {
    /// Appends an attribute value, unescaped.
    fn push(&mut self, raw: &[u8]) -> Result<(), XmlError> {
        value::push_text(raw, &mut self.text)?;
        if self.ends.len() >= MAX_STRINGS || self.text.len() > MAX_TEXT {
            return Err(XmlError::TooLarge);
        }
        self.ends
            .push(u32::try_from(self.text.len()).map_err(|_| XmlError::TooLarge)?);
        Ok(())
    }

    fn clear(&mut self) {
        self.text.clear();
        self.ends.clear();
    }

    /// A copy, in exactly sized allocations (none when empty).
    fn to_strings(&self) -> Strings {
        Strings {
            text: self.text.as_str().into(),
            ends: self.ends.as_slice().into(),
        }
    }
}
