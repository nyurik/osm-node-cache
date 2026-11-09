//! The file header: features, bounding box, writing program and replication position.

use buffa::MessageView as _;

use crate::error::{BlobPosition, PbfError, PbfFormatError, Result};
use crate::generated::osmpbf::OSMPBF::HeaderBlockView;

/// The required features osmflow can read. Any other required feature makes [`Header::parse`] fail.
const SUPPORTED: [&str; 4] = [
    "OsmSchema-V0.6",
    "DenseNodes",
    "HistoricalInformation",
    "LocationsOnWays",
];

/// A bounding box, in nanodegrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    /// The western edge's longitude.
    pub left: i64,
    /// The eastern edge's longitude.
    pub right: i64,
    /// The northern edge's latitude.
    pub top: i64,
    /// The southern edge's latitude.
    pub bottom: i64,
}

/// The header of an OSM PBF file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    /// The area the file covers, if its writer recorded one; objects may still lie outside it.
    pub bbox: Option<BBox>,
    /// Features a reader must understand to read the file correctly, such as `DenseNodes` or
    /// `HistoricalInformation`. Opening a file with one osmflow does not support fails.
    pub required_features: Vec<String>,
    /// Features that tell what the file holds but are safe to ignore, such as `Sort.Type_then_ID`.
    pub optional_features: Vec<String>,
    /// The program that wrote the file, such as `osmium/1.16.0`.
    pub writing_program: Option<String>,
    /// Where the data came from, in the writer's own words.
    pub source: Option<String>,
    /// The replication timestamp, in seconds since 1970, of the diffs that continue this file.
    pub replication_timestamp: Option<i64>,
    /// The replication sequence number of the diffs that continue this file.
    pub replication_sequence: Option<i64>,
    /// The replication server's base URL.
    pub replication_base_url: Option<String>,
}

impl Header {
    /// Parses a decompressed `HeaderBlock`, and checks that its required features are all supported.
    pub(crate) fn parse(data: &[u8], position: BlobPosition) -> Result<Self> {
        let view = HeaderBlockView::decode_view(data)
            .map_err(|e| PbfError::format(position, PbfFormatError::protobuf("file header")(e)))?;
        let header = Self {
            bbox: view.bbox.as_option().map(|b| BBox {
                left: b.left,
                right: b.right,
                top: b.top,
                bottom: b.bottom,
            }),
            required_features: view
                .required_features
                .iter()
                .map(|&s| s.to_owned())
                .collect(),
            optional_features: view
                .optional_features
                .iter()
                .map(|&s| s.to_owned())
                .collect(),
            writing_program: view.writingprogram.map(str::to_owned),
            source: view.source.map(str::to_owned),
            replication_timestamp: view.osmosis_replication_timestamp,
            replication_sequence: view.osmosis_replication_sequence_number,
            replication_base_url: view.osmosis_replication_base_url.map(str::to_owned),
        };
        if let Some(feature) = header
            .required_features
            .iter()
            .find(|f| !SUPPORTED.contains(&f.as_str()))
        {
            return Err(PbfError::UnsupportedFeature(feature.clone()));
        }
        Ok(header)
    }

    /// Whether objects are sorted by type (nodes, then ways, then relations), then by ID.
    #[must_use]
    pub fn is_sorted(&self) -> bool {
        self.optional_features
            .iter()
            .any(|f| f == "Sort.Type_then_ID")
    }

    /// Whether the file holds every version of its objects (a history file), deleted ones included.
    #[must_use]
    pub fn is_history(&self) -> bool {
        self.required_features
            .iter()
            .any(|f| f == "HistoricalInformation")
    }
}
