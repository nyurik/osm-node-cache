//! Node locations for ways: loading a file's nodes into a node location store, looking them up by thread, and, with
//! the `geometry` feature, ways as geo-types geometries.

use std::sync::{Mutex, PoisonError};

#[cfg(feature = "geometry")]
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};

use crate::Location;
#[cfg(feature = "geometry")]
use crate::area::{AreaRules, closed_rings, nest};
use crate::block::{Block, Select, TagMode};
#[cfg(feature = "geometry")]
use crate::block::{Tags, Way};
use crate::error::Result;
use crate::nodes::{BulkLoad as _, NodeRead, NodeReader, NodeStore};
use crate::reader::PbfReader;

/// Why a way has no geometry.
#[cfg(feature = "geometry")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GeometryError {
    /// A node of the way has no location: the store does not have it, as when the way leaves an extract.
    #[error("node {0} has no location")]
    MissingNode(i64),
    /// A line needs two nodes or more.
    #[error("a line needs at least two nodes")]
    TooFewNodes,
    /// An area needs a closed way of four nodes or more, the last at the location of the first.
    #[error("the way is not a closed ring")]
    NotClosed,
    /// No member ways of a relation join into a closed ring of four nodes or more.
    #[error("the member ways join into no closed ring")]
    NoRing,
}

/// Looks up the locations of ways' nodes in a node location store, for one thread. Make one per thread with
/// [`Locator::new`], or get one from [`PbfReader::par_resolve`].
#[derive(Debug)]
pub struct Locator<R> {
    reader: R,
}

/// A location as a geo-types coordinate in degrees.
#[cfg(feature = "geometry")]
fn coord(loc: Location) -> Coord<f64> {
    Coord {
        x: loc.lon_degrees(),
        y: loc.lat_degrees(),
    }
}

impl<R: NodeReader> Locator<R> {
    /// Looks up locations with `reader`, which a store's [`NodeRead::reader`] makes.
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// The location of node `id`, if the store has it.
    pub fn location(&mut self, id: i64) -> Option<Location> {
        self.reader.get(u64::try_from(id).ok()?)
    }

    /// The way as a line: its nodes' locations, in order: those the file stores on the way, or else the store's.
    #[cfg(feature = "geometry")]
    pub fn line_string<T: TagMode>(
        &mut self,
        way: &Way<'_, T>,
    ) -> Result<LineString<f64>, GeometryError> {
        let coords: Vec<Coord<f64>> = match way.locations() {
            // Nanodegrees within ±180° are exact in an `f64`. Dividing rounds correctly where multiplying by an inexact
            // `1e-9` does not, so a location gives the same degrees as the same one in 1e-7 units does.
            #[allow(
                clippy::cast_precision_loss,
                reason = "nanodegrees of a location fit an f64 exactly"
            )]
            Some(locations) => locations
                .map(|(lat, lon)| Coord {
                    x: lon as f64 / 1e9,
                    y: lat as f64 / 1e9,
                })
                .collect(),
            None => way
                .refs()
                .map(|id| {
                    let loc = self.location(id).ok_or(GeometryError::MissingNode(id))?;
                    Ok(coord(loc))
                })
                .collect::<Result<_, _>>()?,
        };
        if coords.len() < 2 {
            return Err(GeometryError::TooFewNodes);
        }
        Ok(LineString::new(coords))
    }

    /// The way as an area without holes: a closed way of four nodes or more.
    #[cfg(feature = "geometry")]
    pub fn polygon<T: TagMode>(&mut self, way: &Way<'_, T>) -> Result<Polygon<f64>, GeometryError> {
        let ring = self.line_string(way)?;
        if ring.0.len() < 4 || !ring.is_closed() {
            return Err(GeometryError::NotClosed);
        }
        Ok(Polygon::new(ring, Vec::new()))
    }

    /// The area of a multipolygon or boundary relation, from the node IDs of its member ways, in any order and
    /// direction. The ways are joined end to end, where they share an end node, into closed rings; chains of ways that
    /// do not close are left out, as are rings of fewer than four nodes. Roles are not read: a ring inside an odd
    /// number of other rings is a hole of the smallest ring around it, and every other ring is an outer ring, so a hole
    /// marked `outer` or an island marked `inner` still comes out right. Outer rings run counterclockwise, holes
    /// clockwise.
    ///
    /// Fails if a node of a closed ring has no location, or if no ring closes.
    #[cfg(feature = "geometry")]
    pub fn multi_polygon<'w>(
        &mut self,
        ways: impl IntoIterator<Item = &'w [i64]>,
    ) -> Result<MultiPolygon<f64>, GeometryError> {
        let mut rings = Vec::new();
        for ids in closed_rings(ways) {
            let coords = ids
                .iter()
                .map(|&id| {
                    let loc = self.location(id).ok_or(GeometryError::MissingNode(id))?;
                    Ok(coord(loc))
                })
                .collect::<Result<Vec<_>, _>>()?;
            rings.push(LineString::new(coords));
        }
        if rings.is_empty() {
            return Err(GeometryError::NoRing);
        }
        Ok(nest(rings))
    }

    /// The way as an area if it is closed and `areas` say its tags make it one, and as a line otherwise.
    #[cfg(feature = "geometry")]
    pub fn area_or_line(
        &mut self,
        way: &Way<'_, Tags>,
        areas: &AreaRules,
    ) -> Result<Geometry<f64>, GeometryError> {
        let line = self.line_string(way)?;
        if line.0.len() >= 4 && line.is_closed() && areas.is_area(way.tags()) {
            Ok(Polygon::new(line, Vec::new()).into())
        } else {
            Ok(line.into())
        }
    }
}

/// The nodes [`PbfReader::load_nodes`] hands a store at once. A block holds about 8,000 nodes, and stores encode only
/// large batches on all threads.
const LOAD_BATCH: usize = 1 << 20;

impl PbfReader {
    /// Loads the locations of the file's nodes into `store`, which must be empty or hold an interrupted load of the
    /// same file, and returns their number. The file must be sorted by type and ID, as published files are; the
    /// store's stamp becomes the file's replication sequence, or 0. Nodes with negative IDs, which only editors'
    /// files have, are left out. Blocks of ways and relations are skipped unread where the file allows it.
    pub fn load_nodes<S: NodeStore>(&self, store: &mut S) -> Result<u64> {
        let stamp = self
            .header()
            .replication_sequence
            .and_then(|s| u64::try_from(s).ok())
            .unwrap_or(0);
        let mut loader = store.bulk_load()?;
        let mut batch = Vec::with_capacity(LOAD_BATCH);
        let mut count = 0;
        for block in self
            .without_filters()
            .par_ordered(Select::node_locations())?
        {
            let block = block?;
            block.push_node_locations(&mut batch);
            if batch.len() >= LOAD_BATCH {
                loader.push_batch(&batch)?;
                count += batch.len() as u64;
                batch.clear();
            }
        }
        loader.push_batch(&batch)?;
        count += batch.len() as u64;
        loader.finish(stamp)?;
        Ok(count)
    }

    /// Calls `f` for every block on the pool, as [`par_unordered`](Self::par_unordered) does, with a [`Locator`] of
    /// `store` for the thread it runs on, to turn ways into geometries.
    pub fn par_resolve<'s, T, S, F>(&self, select: Select<T>, store: &'s S, f: F) -> Result<()>
    where
        T: TagMode,
        S: NodeRead,
        S::Reader<'s>: Send,
        F: Fn(Block<T>, &mut Locator<S::Reader<'s>>) -> Result<()> + Sync,
    {
        // One `Locator` per thread at a time: each keeps the store's blocks it read decoded, for the ways that follow.
        let idle = Mutex::new(Vec::new());
        let take = || idle.lock().unwrap_or_else(PoisonError::into_inner).pop();
        self.par_unordered(select, |block| {
            let mut nodes = take().unwrap_or_else(|| Locator::new(store.reader()));
            let result = f(block, &mut nodes);
            idle.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(nodes);
            result
        })
    }
}
