//! What each way of coding node locations would take, measured on a sorted PBF file's nodes.
//!
//! Nodes are grouped into blocks of `2^shift` IDs (the cache uses 64). The steps between a block's
//! locations are taken from the previous location (MLT's componentwise delta, as in the cache) or
//! from the previous step (MLT's delta2), and coded with:
//! - the cache's codecs: plain, varint or bit-packed, whichever is smallest;
//! - those or patched packing (`FastPFOR`), which stores the steps too wide for its width apart;
//! - MLT's rANS: the steps' bit lengths, signs and top bits entropy coded with tables trained on
//!   the whole file, by MLT's contexts and variants of them, plus 4 bytes a block.

use std::path::Path;

use rayon::prelude::*;

use crate::pbf::{Node, for_each_node};

const FILL: f64 = 0.97;
/// Nodes gathered before their blocks are measured in parallel
const CHUNK: usize = 1 << 24;
const MAX_STEPS: usize = 255;
const PREDICTORS: [&str; 2] = ["delta (now)", "delta2"];
const DEPTHS: usize = 3;
/// The largest symbol, of a 33-bit second difference at depth 2, is `2 * ((33 << 2) | 3) = 270`
const SYMBOLS: usize = 272;
/// Bit lengths above this share the last of the previous-step tables
const CAP: usize = 20;
const SCHEMES: [(&str, usize); 4] = [
    ("rANS, one table", 2),
    ("rANS, MLT", 7),
    ("rANS, MLT no cap", 35),
    ("rANS, + previous", (CAP + 1) * (CAP + 2)),
];
/// Where each scheme's tables start
const OFFSETS: [usize; 4] = [
    0,
    SCHEMES[0].1,
    SCHEMES[0].1 + SCHEMES[1].1,
    SCHEMES[0].1 + SCHEMES[1].1 + SCHEMES[2].1,
];
const CONTEXTS: usize = OFFSETS[3] + SCHEMES[3].1;
const RANS_BLOCK_BYTES: u64 = 4;

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)).cast_unsigned()
}

fn bits(v: u64) -> usize {
    (64 - v.leading_zeros()) as usize
}

fn varint_len(v: u64) -> u64 {
    bits(v).max(1).div_ceil(7) as u64
}

/// MLT's rANS symbol of `step` at `depth`, its count of raw bits, and the bit length of `|step|`
#[expect(clippy::cast_possible_truncation)]
fn symbol(step: i64, depth: usize) -> (usize, u64, usize) {
    if step == 0 {
        return (0, 0, 0);
    }
    let mag = step.unsigned_abs();
    let len = bits(mag);
    let kept = (len - 1).min(depth);
    let raw = len - 1 - kept;
    let mantissa = ((mag >> raw) & ((1 << kept) - 1)) as usize;
    let code = (len << depth) | (mantissa << (depth - kept));
    let sym = if step > 0 { 2 * code - 1 } else { 2 * code };
    (sym, raw as u64, len)
}

/// The longitude's and the latitude's table in each scheme, from the bit lengths of this step's
/// longitude and the previous step's longitude and latitude
fn contexts(lon: usize, prev_lon: usize, prev_lat: usize) -> [(usize, usize); 4] {
    let [_, a, b, c] = OFFSETS;
    [
        (0, 1),
        (a, a + 1 + lon.min(5)),
        (b, b + 1 + lon),
        (
            c + prev_lon.min(CAP),
            c + CAP + 1 + lon.min(CAP) * (CAP + 1) + prev_lat.min(CAP),
        ),
    ]
}

/// Bytes of the zigzag `(lat, lon)` steps at the cache's codecs, and with patched packing too
fn codec_bytes(steps: &[(i64, i64)]) -> (u64, u64) {
    let n = steps.len() as u64;
    let (mut widths, mut varint) = ([[0_u64; 65]; 2], 0);
    for &(lat, lon) in steps {
        for (w, v) in widths.iter_mut().zip([zigzag(lat), zigzag(lon)]) {
            w[bits(v)] += 1;
            varint += varint_len(v);
        }
    }
    let max = widths.map(|w| w.iter().rposition(|&c| c > 0).unwrap_or(0) as u64);
    let packed = 2 + ((max[0] + max[1]) * n).div_ceil(8);
    let cache = (8 * n).min(varint).min(packed);
    let pfor = 2 + (pfor_bits(&widths[0], n) + pfor_bits(&widths[1], n)).div_ceil(8);
    (cache, cache.min(pfor))
}

/// Bits of `n` values with these bit lengths packed at the best width, each wider value stored
/// apart as a position byte and a varint of its higher bits, plus a byte for their count
fn pfor_bits(widths: &[u64; 65], n: u64) -> u64 {
    let max = widths.iter().rposition(|&c| c > 0).unwrap_or(0);
    (0..=max)
        .map(|b| {
            let wider: u64 = (b + 1..=max)
                .map(|w| widths[w] * (8 + 8 * (w - b).div_ceil(7) as u64))
                .sum();
            b as u64 * n + 8 + wider
        })
        .min()
        .unwrap_or(0)
}

struct Stats {
    blocks: u64,
    nodes: u64,
    /// bytes besides the steps: codec byte, table entry, mask, and first location
    other: u64,
    /// bytes of the steps per predictor: at the cache's codecs, and with patched packing
    codecs: [[u64; 2]; 2],
    /// raw bits per predictor and depth
    raw: [u64; 2 * DEPTHS],
    /// symbol counts per predictor, depth and context
    hist: Vec<u64>,
}

impl Stats {
    fn new() -> Self {
        Self {
            blocks: 0,
            nodes: 0,
            other: 0,
            codecs: [[0; 2]; 2],
            raw: [0; 2 * DEPTHS],
            hist: vec![0; 2 * DEPTHS * CONTEXTS * SYMBOLS],
        }
    }

    fn merge(mut self, o: &Self) -> Self {
        self.blocks += o.blocks;
        self.nodes += o.nodes;
        self.other += o.other;
        for (a, b) in self
            .codecs
            .as_flattened_mut()
            .iter_mut()
            .zip(o.codecs.as_flattened())
        {
            *a += b;
        }
        for (a, b) in self
            .raw
            .iter_mut()
            .chain(&mut self.hist)
            .zip(o.raw.iter().chain(&o.hist))
        {
            *a += b;
        }
        self
    }

    fn add(&mut self, block: &[Node], prev_first: (i32, i32), shift: u32) {
        let (first, last) = (block[0], block[block.len() - 1]);
        let n = block.len() as u64;
        self.blocks += 1;
        self.nodes += n;
        let mask = if n == 1 << shift {
            0
        } else if last.id - first.id + 1 == n {
            2
        } else {
            (1 << shift) / 8
        };
        self.other += 5
            + mask
            + varint_len(zigzag(i64::from(first.lat) - i64::from(prev_first.0)))
            + varint_len(zigzag(i64::from(first.lon) - i64::from(prev_first.1)));

        let (mut steps, mut delta2) = ([(0, 0); MAX_STEPS], [(0, 0); MAX_STEPS]);
        let mut prev = (0, 0);
        for ((s, d2), w) in steps.iter_mut().zip(&mut delta2).zip(block.windows(2)) {
            *s = (
                i64::from(w[1].lat) - i64::from(w[0].lat),
                i64::from(w[1].lon) - i64::from(w[0].lon),
            );
            *d2 = (s.0 - prev.0, s.1 - prev.1);
            prev = *s;
        }
        let k = block.len() - 1;
        for (p, vals) in [&steps[..k], &delta2[..k]].into_iter().enumerate() {
            let (cache, pfor) = codec_bytes(vals);
            self.codecs[p][0] += cache;
            self.codecs[p][1] += pfor;
            self.count(p, vals);
        }
    }

    fn count(&mut self, p: usize, vals: &[(i64, i64)]) {
        for d in 0..DEPTHS {
            let base = (p * DEPTHS + d) * CONTEXTS;
            let (mut prev_lat, mut prev_lon) = (0, 0);
            for &(lat, lon) in vals {
                let (slon, rlon, llon) = symbol(lon, d);
                let (slat, rlat, llat) = symbol(lat, d);
                self.raw[p * DEPTHS + d] += rlon + rlat;
                for (clon, clat) in contexts(llon, prev_lon, prev_lat) {
                    self.hist[(base + clon) * SYMBOLS + slon] += 1;
                    self.hist[(base + clat) * SYMBOLS + slat] += 1;
                }
                (prev_lat, prev_lon) = (llat, llon);
            }
        }
    }

    /// Bytes of predictor `p`'s steps in rANS at its best depth, with the tables of `scheme`
    fn rans_bytes(&self, p: usize, scheme: usize) -> f64 {
        (0..DEPTHS)
            .map(|d| {
                let base = (p * DEPTHS + d) * CONTEXTS + OFFSETS[scheme];
                let entropy: f64 = self.hist[base * SYMBOLS..(base + SCHEMES[scheme].1) * SYMBOLS]
                    .chunks(SYMBOLS)
                    .map(|counts| {
                        let total = counts.iter().sum::<u64>() as f64;
                        counts
                            .iter()
                            .filter(|&&c| c > 0)
                            .map(|&c| c as f64 * (total / c as f64).log2())
                            .sum::<f64>()
                    })
                    .sum();
                (entropy + self.raw[p * DEPTHS + d] as f64) / 8.0
            })
            .fold(f64::INFINITY, f64::min)
            + (RANS_BLOCK_BYTES * self.blocks) as f64
    }
}

/// Measures the complete blocks in `buf`, or all of them if `all`, and leaves the rest in it
fn measure(
    stats: &mut [Stats],
    buf: &mut Vec<Node>,
    prev_first: &mut (i32, i32),
    shift: u32,
    all: bool,
) {
    let mut bounds = vec![0];
    bounds.extend((1..buf.len()).filter(|&i| buf[i].id >> shift != buf[i - 1].id >> shift));
    if all {
        bounds.push(buf.len());
    }
    let blocks = bounds.len() - 1;
    if blocks == 0 || buf.is_empty() {
        return;
    }
    let per = blocks.div_ceil(stats.len());
    let (nodes, bounds, before) = (&*buf, &bounds, *prev_first);
    stats.par_iter_mut().enumerate().for_each(|(t, s)| {
        for k in t * per..((t + 1) * per).min(blocks) {
            let prev = if k == 0 {
                before
            } else {
                (nodes[bounds[k - 1]].lat, nodes[bounds[k - 1]].lon)
            };
            s.add(&nodes[bounds[k]..bounds[k + 1]], prev, shift);
        }
    });
    let last = buf[bounds[blocks - 1]];
    *prev_first = (last.lat, last.lon);
    buf.drain(..bounds[blocks]);
}

pub fn run(pbf: &Path, shift: u32) -> anyhow::Result<()> {
    anyhow::ensure!((1..=8).contains(&shift), "blocks must be of 2 to 256 IDs");
    let mut stats: Vec<_> = (0..rayon::current_num_threads())
        .map(|_| Stats::new())
        .collect();
    let mut buf = Vec::with_capacity(CHUNK + 100_000);
    let (mut prev_first, mut lo, mut hi) = ((0, 0), None, 0);
    for_each_node(pbf, |nodes| {
        for n in nodes {
            anyhow::ensure!(
                lo.is_none() || n.id > hi,
                "nodes are not sorted: {} after {hi}",
                n.id
            );
            lo.get_or_insert(n.id);
            hi = n.id;
        }
        buf.extend_from_slice(nodes);
        if buf.len() >= CHUNK {
            measure(&mut stats, &mut buf, &mut prev_first, shift, false);
        }
        Ok(())
    })?;
    measure(&mut stats, &mut buf, &mut prev_first, shift, true);
    let s = stats.iter().fold(Stats::new(), Stats::merge);
    let lo = lo.unwrap_or_default();

    let per = |bytes: f64| bytes / s.nodes as f64;
    let other = s.other as f64;
    println!(
        "{} nodes, IDs {lo}..={hi}, {:.1}% of them present; a dense file of 8 bytes per ID takes {:.1} GB",
        s.nodes,
        100.0 * s.nodes as f64 / (hi - lo + 1) as f64,
        8.0 * (hi + 1) as f64 / 1e9
    );
    println!(
        "{} blocks of {} IDs; {:.2} bytes per node besides the steps",
        s.blocks,
        1 << shift,
        per(other)
    );
    println!(
        "\nbytes per node, and GB at {:.0}% page fill; rANS at its best depth",
        FILL * 100.0
    );
    let names = ["cache codecs", "+ FastPFOR"]
        .into_iter()
        .chain(SCHEMES.map(|s| s.0));
    println!(
        "{:12} | {}",
        "steps",
        names
            .map(|n| format!("{n:>17}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    for (p, name) in PREDICTORS.iter().enumerate() {
        let codecs = s.codecs[p].map(|b| b as f64);
        let rans = (0..SCHEMES.len()).map(|scheme| s.rans_bytes(p, scheme));
        let cells: Vec<String> = codecs
            .into_iter()
            .chain(rans)
            .map(|bytes| {
                let total = other + bytes;
                format!("{:6.3} ({:5.2} GB)", per(total), total / FILL / 1e9)
            })
            .collect();
        println!("{name:12} | {}", cells.join(" | "));
    }
    Ok(())
}
