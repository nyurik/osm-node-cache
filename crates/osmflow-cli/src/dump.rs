//! `osmflow dump`: writes the objects of a PBF file, or the changes of osmChange files, as text or GeoJSON.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use osmflow::nodes::prelude::{NodeRead as _, NodeStore as _};
use osmflow::nodes::{MemoryReader, MemoryStore};
use osmflow::{AreaRules, GeometryError, Header, Location, Locator, PbfReader, Select, Way};

use crate::input::Input;
use crate::select::{ChangeArgs, Filter, Kind, TagSpec, Threads, first_given, parse_tag};
use crate::write::{DumpFormat, Geometry, Props, Writer};
use crate::{changes, output};

/// A file larger than this is dumped whole only with `--all`.
const WHOLE_FILE_LIMIT: u64 = 100 << 20;

#[derive(Debug, clap::Args)]
#[expect(clippy::struct_excessive_bools, reason = "independent flags")]
pub struct Args {
    /// A PBF file, osmChange file, or replication directory or URL.
    ///
    /// An osmChange file is `.osc` or `.osc.gz`, and a replication stream a directory laid out as the servers lay
    /// theirs out, or a server's URL. What the input is is told from its contents, not its name.
    input: PathBuf,
    /// Where to write; standard output when left out, or `-`.
    output: Option<PathBuf>,
    /// The output format; by default, from OUTPUT's extension.
    ///
    /// `.geojson` is GeoJSON, `.geojsonl`, `.geojsons` or `.ndjson` one GeoJSON feature per line, and anything else
    /// text.
    #[arg(short, long)]
    format: Option<DumpFormat>,
    /// In text, each way's geometry as WKT (PBF only).
    ///
    /// The geometry is built from the locations of the way's nodes. GeoJSON always has it.
    #[arg(long)]
    geometry: bool,
    /// In text, each block's layout before its objects (PBF only).
    ///
    /// The layout is the block's blob, string table and groups.
    #[arg(long)]
    blocks: bool,
    /// Each object's version, timestamp, changeset and user.
    #[arg(long)]
    pub metadata: bool,
    /// Stop after this many objects.
    #[arg(long, value_name = "N")]
    pub limit: Option<u64>,
    /// Dump a file over 100 MiB whole.
    ///
    /// Without this flag, such a file needs --id, --tag or --limit.
    #[arg(long)]
    all: bool,
    /// Closed ways with this tag are areas too.
    ///
    /// `KEY` matches any value, and `KEY=V1,V2` one of the values. Repeat the flag for more tags.
    #[arg(long = "area", value_name = "KEY[=VALUES]", value_parser = parse_tag)]
    areas: Vec<TagSpec>,
    /// Closed ways with this tag are lines.
    ///
    /// Another tag can still make them areas. `KEY` matches any value, and `KEY=V1,V2` one of the values.
    #[arg(long = "not-area", value_name = "KEY[=VALUES]", value_parser = parse_tag)]
    not_areas: Vec<TagSpec>,
    /// Ignore the built-in tags that make areas.
    ///
    /// Only `area=yes` and --area then make them areas.
    #[arg(long)]
    no_default_areas: bool,
    #[command(flatten)]
    pub filter: Filter,
    #[command(flatten)]
    pub threads: Threads,
    #[command(flatten)]
    pub changes: ChangeArgs,
}

impl Args {
    fn output(&self) -> Option<&Path> {
        self.output.as_deref().filter(|p| *p != Path::new("-"))
    }

    pub fn format(&self) -> DumpFormat {
        self.format.unwrap_or_else(|| DumpFormat::of(self.output()))
    }

    /// The output, with nothing written yet but the start of a GeoJSON FeatureCollection.
    pub fn writer(&self) -> Result<Writer> {
        let out: Box<dyn Write> = match self.output() {
            Some(path) => Box::new(
                File::create(path).with_context(|| format!("cannot create {}", path.display()))?,
            ),
            None => Box::new(io::stdout().lock()),
        };
        Ok(Writer::new(out, self.format())?)
    }

    /// The first flag given that only PBF files take.
    pub fn pbf_flag(&self) -> Option<&'static str> {
        first_given(&[
            (self.geometry, "--geometry"),
            (self.blocks, "--blocks"),
            (self.all, "--all"),
            (!self.areas.is_empty(), "--area"),
            (!self.not_areas.is_empty(), "--not-area"),
            (self.no_default_areas, "--no-default-areas"),
        ])
    }
}

pub fn run(args: &Args) -> Result<()> {
    match Input::of(&args.input)? {
        Input::Pbf(path) => pbf(args, &path),
        Input::State(path) => bail!(
            "{} is a replication state file, which has no objects; `osmflow stats` shows it",
            path.display()
        ),
        input @ (Input::Osc(_) | Input::Dir(_) | Input::Url(_)) => changes::dump(args, &input),
    }
}

fn pbf(args: &Args, path: &Path) -> Result<()> {
    args.changes.reject_pbf(path)?;
    let format = args.format();
    if args.blocks && format != DumpFormat::Text {
        bail!("--blocks shows blocks in text output only");
    }
    let size = fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    if size > WHOLE_FILE_LIMIT && !args.filter.picks() && args.limit.is_none() && !args.all {
        bail!(
            "{} is {}; give --id, --tag or --limit to choose what to dump, or --all to dump everything",
            path.display(),
            output::size(size)
        );
    }
    let file = args.threads.apply(open(path)?)?;
    let store = if (format != DumpFormat::Text || args.geometry) && args.filter.wants(Kind::Way) {
        Some(node_locations(args, &file)?)
    } else {
        None
    };
    let mut w = args.writer()?;
    header(&mut w, path, size, file.header())?;

    let mut stop = args.filter.id_stop(file.header().is_sorted());
    let file = args.filter.skip_blocks(file);
    let select = args.filter.select(Select::all());
    let select = if args.metadata {
        select
    } else {
        select.without_metadata()
    };
    let matcher = args.filter.matcher();
    let mut locator = store.as_ref().map(|s| Locator::new(s.reader()));
    let areas = args.area_rules();
    let mut left = args.limit.unwrap_or(u64::MAX);
    args.threads.ordered(&file, select, |block| {
        if args.blocks {
            w.block(&block)?;
        }
        for node in block.nodes() {
            if left == 0 {
                return Ok(ControlFlow::Break(()));
            }
            if matcher.matches(Kind::Node, node.id(), node.tags()) {
                let location = Location::new(node.decimicro_lat(), node.decimicro_lon());
                let props = Props::of_info(node.info());
                w.node(node.id(), Some(location), &props, node.tags())?;
                left -= 1;
            }
        }
        for way in block.ways() {
            if left == 0 {
                return Ok(ControlFlow::Break(()));
            }
            if matcher.matches(Kind::Way, way.id(), way.tags()) {
                let geometry = locator.as_mut().map(|l| way_geometry(l, &way, &areas));
                let props = Props::of_info(way.info());
                w.way(way.id(), way.refs(), geometry, &props, way.tags())?;
                left -= 1;
            }
        }
        for relation in block.relations() {
            if left == 0 {
                return Ok(ControlFlow::Break(()));
            }
            if matcher.matches(Kind::Relation, relation.id(), relation.tags()) {
                let props = Props::of_info(relation.info());
                w.relation(relation.id(), relation.members(), &props, relation.tags())?;
                left -= 1;
            }
        }
        Ok(if stop.as_mut().is_some_and(|s| s.block(&block)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })?;
    w.end()?;
    let split = file.split_string_tables();
    if split > 0 {
        writeln!(
            io::stderr(),
            "osmflow: note: {split} blocks of {} have a string table that goes on after their first group, which \
             --tag may have misjudged in the blocks it skipped before finding the first one. Please report the file at \
             https://github.com/nyurik/osmflow/issues",
            path.display()
        )?;
    }
    Ok(())
}

/// A PBF file's header, as `#` lines.
fn header(w: &mut Writer, path: &Path, size: u64, h: &Header) -> io::Result<()> {
    w.comment(|o| write!(o, "file: {} ({})", path.display(), output::size(size)))?;
    if let Some(program) = &h.writing_program {
        w.comment(|o| write!(o, "writing program: {program}"))?;
    }
    if let Some(source) = &h.source {
        w.comment(|o| write!(o, "source: {source}"))?;
    }
    w.comment(|o| write!(o, "required features: {}", h.required_features.join(", ")))?;
    if !h.optional_features.is_empty() {
        w.comment(|o| write!(o, "optional features: {}", h.optional_features.join(", ")))?;
    }
    if let Some(b) = h.bbox {
        w.comment(|o| {
            o.write_all(b"bbox (west, south, east, north): ")?;
            for (i, nano) in [b.left, b.bottom, b.right, b.top].into_iter().enumerate() {
                if i > 0 {
                    o.write_all(b", ")?;
                }
                output::nanodegrees(o, nano)?;
            }
            Ok(())
        })?;
    }
    if let Some(sequence) = h.replication_sequence {
        w.comment(|o| {
            write!(o, "replication: sequence {sequence}")?;
            if let Some(at) = h.replication_timestamp {
                write!(o, ", {at}")?;
            }
            if let Some(url) = &h.replication_base_url {
                write!(o, ", {url}")?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn open(path: &Path) -> Result<PbfReader> {
    PbfReader::open(path).with_context(|| format!("cannot read {}", path.display()))
}

/// The locations of the nodes the dumped ways need. A whole file loads every node; otherwise one pass finds the ways
/// to dump and the nodes they need, and a second keeps only those nodes, so a few ways of the planet need little
/// memory.
fn node_locations(args: &Args, file: &PbfReader) -> Result<MemoryStore> {
    let mut store = MemoryStore::new();
    if args.limit == Some(0) {
        // Nothing is dumped, so no way needs its nodes; the countdown below would also underflow.
        return Ok(store);
    }
    if !args.filter.picks() && args.limit.is_none() {
        file.load_nodes(&mut store)?;
        return Ok(store);
    }
    let matcher = args.filter.matcher();
    let mut needed = HashSet::new();
    let mut left = args.limit.unwrap_or(u64::MAX);
    let mut stop = args.filter.id_stop(file.header().is_sorted());
    let ways = args.filter.skip_blocks(file.clone());
    let select = Select::all()
        .without_nodes()
        .without_relations()
        .without_metadata();
    args.threads.ordered(&ways, select, |block| {
        for way in block.ways() {
            if matcher.matches(Kind::Way, way.id(), way.tags()) {
                needed.extend(way.refs());
                left -= 1;
                if left == 0 {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        // Only ways are read here, so a stop waits for nothing but the ways.
        let done = stop.as_mut().is_some_and(|s| {
            s.passed(Kind::Node, i64::MAX);
            s.passed(Kind::Relation, i64::MAX);
            s.block(&block)
        });
        Ok(if done {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })?;
    if needed.is_empty() {
        return Ok(store);
    }
    args.threads
        .ordered(file, Select::node_locations(), |block| {
            for node in block.nodes() {
                let location = Location::new(node.decimicro_lat(), node.decimicro_lon());
                if needed.remove(&node.id())
                    && location.is_valid()
                    && let Ok(id) = u64::try_from(node.id())
                {
                    store.set(id, location)?;
                }
            }
            Ok(if needed.is_empty() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        })?;
    Ok(store)
}

impl Args {
    /// Which closed ways are areas: the built-in rules, or none, changed by `--area` and `--not-area`.
    fn area_rules(&self) -> AreaRules {
        let mut rules = if self.no_default_areas {
            AreaRules::none()
        } else {
            AreaRules::default()
        };
        for spec in &self.areas {
            rules.add(&spec.key, &values(spec));
        }
        for spec in &self.not_areas {
            rules.remove(&spec.key, &values(spec));
        }
        rules
    }
}

fn values(spec: &TagSpec) -> Vec<&str> {
    spec.values.iter().map(String::as_str).collect()
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "area_or_line makes only lines and polygons"
)]
fn way_geometry(
    locator: &mut Locator<MemoryReader<'_>>,
    way: &Way<'_>,
    areas: &AreaRules,
) -> Result<Geometry, GeometryError> {
    Ok(match locator.area_or_line(way, areas)? {
        geo_types::Geometry::LineString(line) => Geometry::Line(line),
        geo_types::Geometry::Polygon(polygon) => Geometry::Area(polygon),
        other => unreachable!("a way's geometry is a line or a polygon, not {other:?}"),
    })
}
