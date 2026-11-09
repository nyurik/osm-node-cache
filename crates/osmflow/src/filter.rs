//! Block filters: tests of a block's string table, which let a read skip the blocks no wanted object can be in.

use std::fmt;
use std::sync::Arc;

use crate::block::Fields;

/// The strings of one block: every tag key and value, member role and user name its objects use, as its string table
/// holds them. The filters given to [`PbfReader::nodes_where`](crate::PbfReader::nodes_where),
/// [`ways_where`](crate::PbfReader::ways_where) and [`relations_where`](crate::PbfReader::relations_where) test them
/// before the block's objects are decoded.
///
/// A string the block has may be used by any of its objects, as a key, a value, a role or a user name; a string it
/// lacks is used by none. So a test can only tell that a block might hold a wanted object, never that one object is
/// wanted: `!strings.has("x")` means that no object of the block has `x`, not that some object lacks it.
#[derive(Debug, Clone, Copy)]
pub struct BlockStrings<'a> {
    strings: &'a [&'a [u8]],
}

impl BlockStrings<'_> {
    /// Whether some object of the block uses `s`: as a tag key or value, a member role or a user name.
    #[must_use]
    pub fn has(&self, s: &str) -> bool {
        self.strings.contains(&s.as_bytes())
    }

    /// Whether some object of the block uses a string starting with `prefix`, such as the key `name:en` for `name:`.
    #[must_use]
    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.strings
            .iter()
            .any(|string| string.starts_with(prefix.as_bytes()))
    }
}

/// A test of a block's strings: whether the block might hold an object the read wants.
pub(crate) type Filter = Arc<dyn Fn(&BlockStrings<'_>) -> bool + Send + Sync>;

/// The filters of a read, one per object type; a type without one keeps every block.
#[derive(Clone, Default)]
pub(crate) struct Filters {
    pub(crate) nodes: Option<Filter>,
    pub(crate) ways: Option<Filter>,
    pub(crate) relations: Option<Filter>,
}

impl Filters {
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_none() && self.ways.is_none() && self.relations.is_none()
    }

    /// `select` without the object types whose filter rejects a block with these strings.
    pub(crate) fn narrow(&self, select: Fields, strings: &[&[u8]]) -> Fields {
        let strings = BlockStrings { strings };
        let keeps = |filter: &Option<Filter>| filter.as_ref().is_none_or(|f| f(&strings));
        Fields {
            nodes: select.nodes && keeps(&self.nodes),
            ways: select.ways && keeps(&self.ways),
            relations: select.relations && keeps(&self.relations),
            ..select
        }
    }
}

impl fmt::Debug for Filters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Filters")
            .field("nodes", &self.nodes.is_some())
            .field("ways", &self.ways.is_some())
            .field("relations", &self.relations.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Select;

    const STRINGS: [&[u8]; 4] = [b"", b"highway", b"name:en", b"Main St"];

    #[test]
    fn strings_are_tested_whole_or_by_prefix() {
        let strings = BlockStrings { strings: &STRINGS };
        assert!(strings.has("highway") && strings.has("Main St"));
        assert!(!strings.has("high") && !strings.has("name"));
        assert!(strings.has_prefix("name:") && strings.has_prefix("high"));
        assert!(!strings.has_prefix("building"));
    }

    #[test]
    fn narrowing_drops_only_rejected_types() {
        let all = Select::all().fields();
        let filters = Filters {
            ways: Some(Arc::new(|s| s.has("building"))),
            relations: Some(Arc::new(|s| s.has("highway"))),
            ..Filters::default()
        };
        let narrowed = filters.narrow(all, &STRINGS);
        assert!(narrowed.nodes && !narrowed.ways && narrowed.relations && narrowed.tags);
        assert!(Filters::default().is_empty() && !filters.is_empty());
        assert_eq!(
            format!("{filters:?}"),
            "Filters { nodes: false, ways: true, relations: true }"
        );
    }
}
