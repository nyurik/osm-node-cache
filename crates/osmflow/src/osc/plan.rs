//! Finding the diff of a stream that a time falls in, and planning a catch-up over streams of several granularities
//! (day, hour, minute) that downloads each change once.
//!
//! Both work on state files alone, read through [`StateSource`], so they do no I/O of their own: a [`Replication`]
//! reads them from a directory or a server, and a program can read them from wherever it keeps them.
//!
//! [`Replication`]: crate::osc::Replication

use std::io;
use std::ops::RangeInclusive;

use tracing::{debug, debug_span, trace};

use crate::osc::{OscError, Replication, State};

/// How long after its timestamp a change may first show in a diff, in milliseconds: ten minutes.
///
/// The API server gives a change its timestamp while it saves it, and the change shows in the first diff cut after
/// the save commits, so a change can be older than diffs cut before it. A diff's state is the newest timestamp of its
/// changes, so a state can also be older than the one before it, by as much. On the planet's minute stream, over the
/// 1842 diffs 7364100 to 7365941 of November 2026, the latest change came 42 seconds after the state of the diff
/// before its own, and no state was older than the one before it; the OSM web servers end a request after 300
/// seconds.
pub const LATE_MS: i64 = 10 * 60_000;

/// The state files of one replication stream: what [`locate`] and [`plan`] read.
///
/// A stream's states are expected in one run of sequences, up to [`latest`](Self::latest): older ones may be gone,
/// as servers delete old diffs, but none is missing in between. Their timestamps go up with their sequences, or down
/// by [`LATE_MS`] at most.
pub trait StateSource {
    /// The stream's latest state.
    fn latest(&self) -> Result<State, OscError>;

    /// The state of diff `sequence`, or `None` if the stream does not have it: deleted as too old, or not published
    /// yet. Errors are for failures to read, which end a search.
    fn get(&self, sequence: u64) -> Result<Option<State>, OscError>;
}

/// The states of a stream read through a [`Replication`]: a state file it has not cached is downloaded once, then
/// read from the cache. A state the directory or the server does not have is `None`.
impl StateSource for Replication {
    fn latest(&self) -> Result<State, OscError> {
        self.state()
    }

    fn get(&self, sequence: u64) -> Result<Option<State>, OscError> {
        match self.sequence_state(sequence) {
            Ok(state) => Ok(Some(state)),
            Err(OscError::NotFound { .. } | OscError::NotPublished { .. }) => Ok(None),
            Err(OscError::File { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

/// Where a time falls in a stream, from [`locate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Located {
    /// The last state at or before the time.
    At(State),
    /// The time is before the oldest state the stream still has, given here: what came before it is not known.
    BeforeOldest(State),
}

/// Finds where `timestamp_ms`, in milliseconds since 1970, falls in a stream: the last state at or before it.
///
/// It reads state files only, never a diff: the latest state, then a few others. Diffs come at nearly fixed intervals,
/// so the number of the one sought is estimated from the times of the states read so far, which takes 2 to 4 more
/// reads on a regular stream. Each estimate that misses doubles the next step, and a range that an estimate does not
/// halve is halved instead, so a stream with outages takes at most about twice the reads of a binary search: some 50
/// for the planet's minute stream. Where states are older than ones before them, the state found is at or before the
/// time, though a later one may be too.
///
/// The changes made after a time are not all in the diffs after its state, as changes show up to [`LATE_MS`] late:
/// [`plan`] starts that much earlier from a snapshot's time.
pub fn locate(
    source: &(impl StateSource + ?Sized),
    timestamp_ms: i64,
) -> Result<Located, OscError> {
    let latest = source.latest()?;
    locate_from(source, latest, timestamp_ms)
}

/// [`locate`], from the stream's latest state.
fn locate_from(
    source: &(impl StateSource + ?Sized),
    latest: State,
    t: i64,
) -> Result<Located, OscError> {
    let _span = debug_span!("locate", timestamp_ms = t).entered();
    let located = search(source, latest, t)?;
    match located {
        Located::At(state) => {
            debug!(
                sequence = state.sequence,
                state_ms = state.timestamp_ms,
                "located"
            );
        }
        Located::BeforeOldest(oldest) => {
            debug!(oldest = oldest.sequence, "before the oldest state");
        }
    }
    Ok(located)
}

/// [`locate_from`], without its span.
fn search(
    source: &(impl StateSource + ?Sized),
    latest: State,
    t: i64,
) -> Result<Located, OscError> {
    if latest.timestamp_ms <= t {
        return Ok(Located::At(latest));
    }
    // `hi` is always a state after `t`.
    let mut hi = latest;

    // First a state at or before `t`, stepping back from the latest. Once a probe finds no state, the oldest one is
    // somewhere between it and `hi`, and the probes halve that range.
    let mut missing: Option<u64> = None;
    let mut step = 1;
    let mut rate = None;
    let mut lo = loop {
        let floor = missing.map_or(0, |m| m + 1);
        if hi.sequence <= floor {
            return Ok(Located::BeforeOldest(hi));
        }
        let probe = match (missing, rate) {
            (Some(m), _) => m + (hi.sequence - m) / 2,
            (None, Some(ms_per_diff)) => {
                // Aim a little past the estimate, so the probe is most likely before `t`.
                let estimate = sequences_in(hi.timestamp_ms - t, ms_per_diff);
                let estimate = estimate + estimate / 256 + 2;
                step = estimate.max(step * 2);
                hi.sequence.saturating_sub(step).max(floor)
            }
            (None, None) => hi.sequence - 1,
        };
        let state = source.get(probe)?;
        trace!(probe, ?state, "probe before the time");
        match state {
            Some(state) if state.timestamp_ms <= t => break state,
            Some(state) => {
                rate = ms_per_diff(state, hi).or(rate);
                hi = state;
            }
            None => missing = Some(probe),
        }
    };

    // Then the range between them, narrowed by estimates, or halved when an estimate did not halve it.
    let mut halve = false;
    while hi.sequence - lo.sequence > 1 {
        let width = hi.sequence - lo.sequence;
        let probe = if halve {
            lo.sequence + width / 2
        } else {
            let estimate = interpolate(lo, hi, t);
            estimate.clamp(lo.sequence + 1, hi.sequence - 1)
        };
        let state = source.get(probe)?;
        trace!(probe, ?state, "probe between");
        let Some(state) = state else {
            return Err(OscError::StateGap { sequence: probe });
        };
        if state.timestamp_ms <= t {
            lo = state;
        } else {
            hi = state;
        }
        halve = !halve && (hi.sequence - lo.sequence) * 2 > width;
    }
    Ok(Located::At(lo))
}

/// The milliseconds per diff between two states, `a` before `b`; `None` if they are out of order.
#[expect(clippy::cast_precision_loss, reason = "an estimate")]
fn ms_per_diff(a: State, b: State) -> Option<f64> {
    let (diffs, ms) = (
        b.sequence.checked_sub(a.sequence)?,
        b.timestamp_ms - a.timestamp_ms,
    );
    (diffs > 0 && ms > 0).then(|| ms as f64 / diffs as f64)
}

/// The number of diffs that take `ms` at `ms_per_diff`, rounded up.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "an estimate, which saturates"
)]
fn sequences_in(ms: i64, ms_per_diff: f64) -> u64 {
    (ms as f64 / ms_per_diff).ceil() as u64
}

/// The sequence `t` falls at between `lo` and `hi`, as if diffs came at even intervals between them.
fn interpolate(lo: State, hi: State, t: i64) -> u64 {
    let (span, offset) = (
        i128::from(hi.timestamp_ms - lo.timestamp_ms),
        i128::from(t - lo.timestamp_ms),
    );
    let width = i128::from(hi.sequence - lo.sequence);
    let at = i128::from(lo.sequence) + offset * width / span.max(1);
    u64::try_from(at).unwrap_or(lo.sequence)
}

/// Where a [`plan`] starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// After a state of one of the streams, by its index in the list given to [`plan`]: where a step of an earlier
    /// plan ended, or the replication sequence in a file's header, as Geofabrik's extracts give it.
    After {
        /// The stream's index.
        stream: usize,
        /// The state of the last diff already applied.
        state: State,
    },
    /// After a time, in milliseconds since 1970: every change made after it, and none made at or before it, from the
    /// first diff whose state is after it. A diff's state is the newest timestamp of its changes, so each change made
    /// after the time is in such a diff.
    Time {
        /// The time.
        timestamp_ms: i64,
    },
    /// After a snapshot of the database whose newest change has this timestamp, in milliseconds since 1970: the
    /// replication timestamp in a planet file's header, which has no sequence. A change the snapshot does not have
    /// committed after it was taken, and can be up to [`LATE_MS`] older than that, so the plan starts as
    /// [`Start::Time`] does from `LATE_MS` before the timestamp, and its first diffs may repeat changes the snapshot
    /// has.
    Snapshot {
        /// The newest timestamp in the snapshot.
        timestamp_ms: i64,
    },
}

/// A run of diffs of one stream, from [`plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The stream's index in the list given to [`plan`].
    pub stream: usize,
    /// The state the first diff continues from: of the diff before it.
    pub from: State,
    /// The state of the last diff.
    pub to: State,
}

impl Step {
    /// The diffs to read, as [`Replication::diffs`] takes them.
    #[must_use]
    pub fn sequences(&self) -> RangeInclusive<u64> {
        self.from.sequence + 1..=self.to.sequence
    }
}

/// Plans reading the changes after `start` up to `to_ms` (or the latest state of the last stream), over `streams`
/// ordered from the coarsest to the finest, such as the planet's day, hour and minute streams. Finer diffs lead up to
/// where a coarser stream's diff starts, as many whole coarser diffs follow as fit, and finer ones again cover the
/// rest. Ten days behind on the planet is then about 10 day diffs and some tens of hour and minute diffs on either
/// side, instead of 14,400 minute diffs. The last step's stream and `to` are the [`Start::After`] of the next plan.
///
/// Each coarser stream is expected to be made from the next finer one as `osmosis --merge-replication-files` makes
/// the planet's: a coarser diff holds the finer diffs after the ones the diff before it holds, up to the last before
/// the first whose state is after its own, and its state's timestamp is on its interval. Where it ends in the finest
/// stream is then found from the finest states around its time, and no change is read twice. After an outage of the
/// stream it is merged from, longer than its interval, the coarser diff in which that stream resumes may have been
/// cut early, holding only the first diff after the outage, which no state shows: where the plan steps down to finer
/// diffs there, it starts them from that diff, and where it steps up, the finer diffs it reads first may be in the
/// coarser diffs too. The changes read twice come as whole versions of their objects, so applying them again changes
/// nothing, and a reader that counts changes drops each (type, ID, version) it has seen.
///
/// Only state files are read, never a diff: each stream's latest state, then some around each place the plan changes
/// streams, which a [`Replication`] caches: some 130 for 10 days of the planet's three streams, in this module's tests.
/// A coarser stream that has no whole diff in a stretch is left out of it. The finest stream must go back to the
/// start, or the plan is an [`OscError::TooOld`] error.
///
/// # Panics
///
/// If a [`Start::After`] names a stream that is not in `streams`.
///
/// ```
/// use std::path::Path;
///
/// use osmflow::osc::{OscError, Replication, Start, StateSource, plan};
///
/// /// Applies the planet's changes after `start`, kept in `dir/day`, `dir/hour` and `dir/minute`; with the `http`
/// /// feature, `Replication::http` downloads them there from `https://planet.openstreetmap.org/replication/day` and
/// /// so on. Returns where the next catch-up starts.
/// fn catch_up(dir: &Path, start: Start) -> Result<Start, OscError> {
///     let streams = ["day", "hour", "minute"].map(|s| Replication::local(dir.join(s)));
///     let sources: Vec<&dyn StateSource> = streams.iter().map(|s| s as &dyn StateSource).collect();
///     let mut reached = start;
///     for step in plan(&sources, start, None)? {
///         for diff in streams[step.stream].diffs(step.sequences())? {
///             let diff = diff?;
///             // ... apply `diff.changes`, then keep the position reached:
///             reached = Start::After { stream: step.stream, state: diff.state };
///         }
///     }
///     Ok(reached)
/// }
/// ```
pub fn plan(
    streams: &[&dyn StateSource],
    start: Start,
    to_ms: Option<i64>,
) -> Result<Vec<Step>, OscError> {
    let _span = debug_span!("plan", streams = streams.len(), ?start, to_ms).entered();
    let Some((&finest, coarser)) = streams.split_last() else {
        return Ok(Vec::new());
    };
    let fine = Fine(Known::new(coarser.len(), finest)?);
    let coarser = coarser
        .iter()
        .enumerate()
        .map(|(index, &source)| Coarse::new(index, source))
        .collect::<Result<Vec<_>, _>>()?;
    let from = match start {
        Start::Time { timestamp_ms } => fine.after(timestamp_ms)?,
        Start::Snapshot { timestamp_ms } => fine.after(timestamp_ms - LATE_MS)?,
        Start::After { stream, state } if stream == fine.0.index => state,
        Start::After { stream, state } => {
            let coarse = coarser
                .get(stream..)
                .expect("the start's stream is one of the streams");
            match fine.end(state.timestamp_ms)? {
                Some(end) => fine.resumed(coarse, state.timestamp_ms, end.low)?,
                None => return Err(fine.too_old(state.timestamp_ms)?),
            }
        }
    };
    let to = match to_ms {
        None => fine.0.latest,
        Some(t) => match fine.0.locate(t)? {
            Located::At(state) => state,
            Located::BeforeOldest(_) => return Ok(Vec::new()),
        },
    };
    let mut steps = Vec::new();
    cover(&fine, &coarser, from, to, &mut steps)?;
    for step in &steps {
        debug!(
            stream = step.stream,
            first = step.from.sequence + 1,
            last = step.to.sequence,
            "step"
        );
    }
    Ok(steps)
}

/// A stream, its index in the plan's list, and its latest state.
struct Known<'a> {
    index: usize,
    source: &'a dyn StateSource,
    latest: State,
}

impl<'a> Known<'a> {
    fn new(index: usize, source: &'a dyn StateSource) -> Result<Self, OscError> {
        Ok(Self {
            index,
            source,
            latest: source.latest()?,
        })
    }

    fn locate(&self, t: i64) -> Result<Located, OscError> {
        locate_from(self.source, self.latest, t)
    }

    /// The state after `state`, if the stream has one yet.
    fn next(&self, state: State) -> Result<Option<State>, OscError> {
        if state.sequence >= self.latest.sequence {
            return Ok(None);
        }
        let sequence = state.sequence + 1;
        match self.source.get(sequence)? {
            Some(next) => Ok(Some(next)),
            None => Err(OscError::StateGap { sequence }),
        }
    }
}

/// A coarser stream, and the length of its diffs, which its states are that far apart: `None` if it has one state.
struct Coarse<'a> {
    known: Known<'a>,
    interval_ms: Option<i64>,
}

impl<'a> Coarse<'a> {
    fn new(index: usize, source: &'a dyn StateSource) -> Result<Self, OscError> {
        let known = Known::new(index, source)?;
        let before = match known.latest.sequence.checked_sub(1) {
            Some(sequence) => source.get(sequence)?,
            None => None,
        };
        let interval_ms = before.map(|b| known.latest.timestamp_ms - b.timestamp_ms);
        Ok(Self { known, interval_ms })
    }
}

/// Where a coarser diff ends in the finest stream: `low` is never after the last finest diff it holds, and `high`
/// never before; they are the same diff unless a finest state is at the coarser one's time.
struct End {
    low: State,
    high: State,
}

/// The finest stream, whose diffs every other stream's diffs are made of.
struct Fine<'a>(Known<'a>);

impl Fine<'_> {
    /// The state before the first diff whose state is after `t`, found as [`Fine::end`] finds where a coarser diff at
    /// `t` ends.
    fn after(&self, t: i64) -> Result<State, OscError> {
        match self.end(t)? {
            Some(end) => Ok(end.high),
            None => Err(self.too_old(t)?),
        }
    }

    /// The error for a position at `t` that this stream does not go back to.
    fn too_old(&self, t: i64) -> Result<OscError, OscError> {
        Ok(match self.0.locate(t - LATE_MS)? {
            Located::BeforeOldest(oldest) => OscError::TooOld {
                timestamp_ms: t,
                oldest,
            },
            Located::At(state) => OscError::TooOld {
                timestamp_ms: t,
                oldest: state,
            },
        })
    }

    /// Where a coarser diff whose state is at `t` ends in this stream; `None` if this stream does not go back to it.
    fn end(&self, t: i64) -> Result<Option<End>, OscError> {
        // A state at or before `t - LATE_MS` is before every finest diff after the coarser one, so the coarser diff
        // ends before the first state after it that is after `t`; a run of the merge that reached a state at `t`
        // ended it there.
        let Located::At(mut high) = self.0.locate(t - LATE_MS)? else {
            return Ok(None);
        };
        let mut low = None;
        while let Some(next) = self.0.next(high)?
            && next.timestamp_ms <= t
        {
            high = next;
            if high.timestamp_ms == t {
                low = low.or(Some(high));
            }
        }
        Ok(Some(End {
            low: low.unwrap_or(high),
            high,
        }))
    }

    /// `low`, the `End::low` of the diff of `coarse[0]` whose state is at `t`, or where the stream it is merged from,
    /// `coarse[1]` or this one, resumed after an outage longer than the diffs of `coarse[0]`: a run of the merge that
    /// starts with such a diff writes it alone into the coarser diff, ending it early.
    fn resumed(&self, coarse: &[Coarse<'_>], t: i64, low: State) -> Result<State, OscError> {
        let merged = coarse.get(1).map_or(&self.0, |c| &c.known);
        if let Some(interval) = coarse[0].interval_ms
            && let Located::At(before) = merged.locate(t - interval)?
            && let Some(resumed) = merged.next(before)?
            && resumed.timestamp_ms - before.timestamp_ms > interval
        {
            let resumed = if coarse.len() > 1 {
                self.end(resumed.timestamp_ms)?.map(|end| end.low)
            } else {
                Some(resumed)
            };
            if let Some(resumed) = resumed
                && resumed.sequence < low.sequence
            {
                return Ok(resumed);
            }
        }
        Ok(low)
    }
}

/// Appends the steps that read the finest diffs after `from` up to `to`, with `coarser` and the finest stream.
fn cover(
    fine: &Fine<'_>,
    coarser: &[Coarse<'_>],
    from: State,
    to: State,
    steps: &mut Vec<Step>,
) -> Result<(), OscError> {
    if from.sequence >= to.sequence {
        return Ok(());
    }
    let Some((coarse, finer)) = coarser.split_first() else {
        steps.push(Step {
            stream: fine.0.index,
            from,
            to,
        });
        return Ok(());
    };
    if let Some((first, lead, last, tail)) = whole(fine, coarser, from, to)? {
        cover(fine, finer, from, lead, steps)?;
        steps.push(Step {
            stream: coarse.known.index,
            from: first,
            to: last,
        });
        return cover(fine, finer, tail, to, steps);
    }
    cover(fine, finer, from, to, steps)
}

/// The whole diffs of `coarse[0]` between finest states `from` and `to`, as the states before the first and of the
/// last, with the finest states the finer diffs before them lead up to, and the finer diffs after them start from.
fn whole(
    fine: &Fine<'_>,
    coarse: &[Coarse<'_>],
    from: State,
    to: State,
) -> Result<Option<(State, State, State, State)>, OscError> {
    let known = &coarse[0].known;
    // The first coarse diff that starts at or after `from`: the one after the first that does not end before it. One
    // that ends at `from` is most likely at its time. State 0 of a merged stream has no diff, and the diff after it
    // starts where the merge began, before state 0's time.
    let mut first = match known.locate(from.timestamp_ms)? {
        Located::At(state) | Located::BeforeOldest(state) => state,
    };
    let lead = loop {
        if first.sequence > 0
            && let Some(end) = fine.end(first.timestamp_ms)?
            && end.high.sequence >= from.sequence
        {
            break end.high;
        }
        match known.next(first)? {
            Some(next) => first = next,
            None => return Ok(None),
        }
    };
    // The last that ends at or before `to`.
    let Located::At(mut last) = known.locate(to.timestamp_ms + LATE_MS)? else {
        return Ok(None);
    };
    let tail = loop {
        if last.sequence <= first.sequence {
            return Ok(None);
        }
        if let Some(end) = fine.end(last.timestamp_ms)?
            && end.high.sequence <= to.sequence
        {
            break fine.resumed(coarse, last.timestamp_ms, end.low)?;
        }
        match known.source.get(last.sequence - 1)? {
            Some(before) => last = before,
            None => return Ok(None),
        }
    };
    Ok(Some((first, lead, last, tail)))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::rc::Rc;

    use rand::rngs::StdRng;
    use rand::{RngExt as _, SeedableRng as _};

    use super::*;

    const SECOND: i64 = 1000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    fn ms(n: u64) -> i64 {
        i64::try_from(n).unwrap()
    }

    fn index(n: u64) -> usize {
        usize::try_from(n).unwrap()
    }

    /// A stream computed rather than stored, counting the states read: diffs `first..=last`, the state of `n` at
    /// `time(n)`, or missing where `time` gives `None`.
    struct Stream {
        first: u64,
        last: u64,
        time: Box<dyn Fn(u64) -> Option<i64>>,
        reads: Cell<usize>,
        /// The states read, which a `Replication` downloads once each.
        files: RefCell<BTreeSet<u64>>,
    }

    impl Stream {
        fn new(
            sequences: RangeInclusive<u64>,
            time: impl Fn(u64) -> Option<i64> + 'static,
        ) -> Self {
            Self {
                first: *sequences.start(),
                last: *sequences.end(),
                time: Box::new(time),
                reads: Cell::new(0),
                files: RefCell::default(),
            }
        }

        /// A stream whose every diff ends at `time(n)`.
        fn every(sequences: RangeInclusive<u64>, time: impl Fn(u64) -> i64 + 'static) -> Self {
            Self::new(sequences, move |n| Some(time(n)))
        }

        /// A stream of diffs `first..`, at `times`.
        fn of(first: u64, times: Rc<[i64]>) -> Self {
            let last = first + times.len() as u64 - 1;
            Self::new(first..=last, move |n| {
                times.get(index(n.checked_sub(first)?)).copied()
            })
        }

        fn state(&self, sequence: u64) -> State {
            let timestamp_ms = (self.time)(sequence).expect("the stream has the state");
            State {
                sequence,
                timestamp_ms,
            }
        }

        /// The answer of a search, found by reading every state from the latest back.
        fn expected(&self, t: i64) -> Located {
            match (self.first..=self.last)
                .rev()
                .find(|&n| (self.time)(n).is_some_and(|ts| ts <= t))
            {
                Some(n) => Located::At(self.state(n)),
                None => Located::BeforeOldest(self.state(self.first)),
            }
        }
    }

    impl StateSource for Stream {
        fn latest(&self) -> Result<State, OscError> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.state(self.last))
        }

        fn get(&self, sequence: u64) -> Result<Option<State>, OscError> {
            self.reads.set(self.reads.get() + 1);
            self.files.borrow_mut().insert(sequence);
            let kept = (self.first..=self.last).contains(&sequence);
            Ok(kept
                .then(|| (self.time)(sequence))
                .flatten()
                .map(|timestamp_ms| State {
                    sequence,
                    timestamp_ms,
                }))
        }
    }

    /// Searches `stream` for `t`, checks the answer, and returns the number of states read.
    fn reads(stream: &Stream, t: i64) -> usize {
        stream.reads.set(0);
        assert_eq!(locate(stream, t).unwrap(), stream.expected(t), "time {t}");
        stream.reads.get()
    }

    #[test]
    fn a_regular_stream_takes_a_few_reads() {
        // The planet's minute stream: some 7.4 million diffs, one a minute, with a few seconds of jitter.
        let minute = Stream::every(0..=7_400_000, |n| ms(n) * MINUTE + ms(n % 7) * 1000);
        let latest = minute.state(7_400_000).timestamp_ms;
        for back in [
            0,
            1,
            MINUTE / 2,
            MINUTE,
            90 * MINUTE,
            10 * DAY,
            400 * DAY,
            4000 * DAY,
        ] {
            let n = reads(&minute, latest - back);
            assert!(n <= 6, "{n} reads for {back} ms back");
        }
        let fifth = minute.state(5);
        assert_eq!(
            locate(&minute, fifth.timestamp_ms).unwrap(),
            Located::At(fifth)
        );
        assert_eq!(
            locate(&minute, -1).unwrap(),
            Located::BeforeOldest(minute.state(0))
        );
    }

    #[test]
    fn outages_repeated_times_and_deleted_diffs_are_searched_too() {
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..50 {
            // The oldest diffs deleted, outages of up to a week, and diffs at the same time as the one before.
            let first = rng.random_range(0..2000);
            let mut t = 0;
            let times: Vec<i64> = (0..=50_000)
                .map(|_| {
                    t += match rng.random_range(0..1000) {
                        0 => rng.random_range(HOUR..7 * DAY),
                        1..10 => 0,
                        _ => rng.random_range(50_000..70_000),
                    };
                    t
                })
                .collect();
            let (oldest, latest) = (times[index(first)], t);
            let stream = Stream::new(first..=50_000, move |n| times.get(index(n)).copied());
            for _ in 0..40 {
                let n = reads(&stream, rng.random_range(oldest - DAY..latest + DAY));
                assert!(n <= 60, "{n} reads");
            }
        }
    }

    #[test]
    fn a_missing_state_between_others_is_an_error() {
        // The first estimate lands below the gap, and the next one in it.
        let stream = Stream::new(0..=1000, |n| (n != 500).then(|| ms(n) * MINUTE));
        assert!(matches!(
            locate(&stream, 500 * MINUTE + MINUTE / 2),
            Err(OscError::StateGap { sequence: 500 })
        ));
    }

    /// A coarser stream made from a finer one, run by run, as `osmosis --merge-replication-files` makes it.
    #[derive(Default)]
    struct Merged {
        /// The timestamp of each coarser state, from sequence 0.
        times: Vec<i64>,
        /// The last finer diff each coarser diff holds.
        ends: Vec<u64>,
        /// When each coarser state was published: at the end of the run that wrote it.
        published: Vec<i64>,
    }

    impl Merged {
        fn push(&mut self, time: i64, end: usize, run: i64) {
            self.times.push(time);
            self.ends.push(end as u64);
            self.published.push(run);
        }
    }

    /// Merges the finer diffs from sequence 0 at `times`, published at `published`, into diffs of `interval`, running
    /// at `runs`.
    fn merge(
        times: &[i64],
        published: &[i64],
        interval: i64,
        runs: impl IntoIterator<Item = i64>,
    ) -> Merged {
        let align = |t: i64| t - t.rem_euclid(interval);
        let mut merged = Merged::default();
        // The finer diff the merge has read up to, and the end of the coarser one it writes.
        let mut local: Option<usize> = None;
        let mut end = 0;
        for run in runs {
            let Some(server) = published.iter().rposition(|&p| p <= run) else {
                continue;
            };
            let Some(start) = local else {
                // The first run writes state 0, at the next interval boundary.
                let at = align(times[server]);
                end = if at < times[server] {
                    at + interval
                } else {
                    at
                };
                merged.push(end, server, run);
                local = Some(server);
                continue;
            };
            let mut maximum = times[server];
            if maximum - times[start] > 10 * interval {
                maximum = times[start] + 10 * interval;
            }
            maximum = align(maximum);
            if merged.times.len() == 1 && maximum <= end {
                maximum -= interval;
            }
            if maximum > times[start] && maximum <= end {
                maximum = times[start];
            }
            let mut open = false;
            let mut at = start;
            while at < server && times[at] < maximum {
                let next = at + 1;
                if times[next] > maximum && at != start {
                    break;
                }
                if !open {
                    end = align(end + interval);
                    open = true;
                }
                while times[next] > end {
                    merged.push(end, at, run);
                    end += interval;
                }
                at = next;
            }
            if open {
                merged.push(end, at, run);
            }
            local = Some(at);
        }
        merged
    }

    /// The planet's streams: minutes at `minutes`, published 20 seconds after their states, and hours and days merged
    /// from them every 15 minutes.
    struct Planet {
        minutes: Rc<[i64]>,
        hours: Merged,
        days: Merged,
    }

    impl Planet {
        fn new(minutes: Vec<i64>) -> Self {
            let published: Vec<i64> = minutes.iter().map(|t| t + 20 * SECOND).collect();
            let until = published[published.len() - 1] + DAY;
            let runs = || {
                (0..)
                    .map(|k| k * 15 * MINUTE + 2 * MINUTE)
                    .take_while(move |&r| r < until)
            };
            let hours = merge(&minutes, &published, HOUR, runs());
            // The day merge runs after the hour merge, in the same minute.
            let days = merge(&hours.times, &hours.published, DAY, runs());
            Self {
                minutes: Rc::from(minutes),
                hours,
                days,
            }
        }

        fn streams(&self) -> [Stream; 3] {
            [
                Stream::of(0, Rc::from(self.days.times.as_slice())),
                Stream::of(0, Rc::from(self.hours.times.as_slice())),
                Stream::of(0, Rc::clone(&self.minutes)),
            ]
        }

        /// The last minute diff the diff `sequence` of stream `stream` holds.
        fn end(&self, stream: usize, sequence: u64) -> u64 {
            match stream {
                0 => self.hours.ends[index(self.days.ends[index(sequence)])],
                1 => self.hours.ends[index(sequence)],
                _ => sequence,
            }
        }

        /// Checks that `steps` read every minute diff after `from` up to `to` and returns how many they read twice.
        fn check(&self, steps: &[Step], from: u64, to: u64) -> u64 {
            let mut reached = from;
            let mut twice = 0;
            for step in steps {
                let (start, end) = (
                    self.end(step.stream, step.from.sequence),
                    self.end(step.stream, step.to.sequence),
                );
                assert!(
                    start <= reached,
                    "minutes {reached}..{start} are missed: {steps:?}"
                );
                assert!(end <= to, "{steps:?}");
                twice += reached.min(end) - start;
                reached = reached.max(end);
            }
            assert_eq!(reached, to.max(from), "{steps:?}");
            twice
        }
    }

    /// Minutes, each cut 0 to 59 seconds into its minute, for `days`.
    fn minutes(rng: &mut StdRng, days: i64) -> Vec<i64> {
        (1..days * 1440)
            .map(|m| m * MINUTE + rng.random_range(0..60) * SECOND)
            .collect()
    }

    fn dyns(streams: &[Stream; 3]) -> [&dyn StateSource; 3] {
        [&streams[0], &streams[1], &streams[2]]
    }

    fn after(stream: usize, state: State) -> Start {
        Start::After { stream, state }
    }

    #[test]
    fn the_merge_model_cuts_hours_at_their_ends() {
        let mut rng = StdRng::seed_from_u64(3);
        let planet = Planet::new(minutes(&mut rng, 3));
        let hours = planet.hours.times.iter().zip(&planet.hours.ends);
        for (h, (&t, &end)) in hours.enumerate().skip(1) {
            assert_eq!(t % HOUR, 0);
            let (last, next) = (index(end), index(end) + 1);
            assert!(planet.minutes[last] <= t, "hour {h}");
            assert!(planet.minutes.get(next).is_none_or(|&n| n > t), "hour {h}");
        }
        assert_eq!(planet.days.times[1] % DAY, 0);
    }

    #[test]
    fn a_catch_up_takes_days_then_hours_then_minutes() {
        let mut rng = StdRng::seed_from_u64(4);
        let planet = Planet::new(minutes(&mut rng, 30));
        let streams = planet.streams();
        let latest = streams[2].latest().unwrap();
        // 10 days, 5 hours and 7 minutes behind.
        let from = streams[2].state(latest.sequence - (10 * 1440 + 5 * 60 + 7));
        let steps = plan(&dyns(&streams), after(2, from), None).unwrap();
        let shape: Vec<_> = steps.iter().map(|s| s.stream).collect();
        assert_eq!(shape, [2, 1, 0, 1, 2]);
        assert_eq!(planet.check(&steps, from.sequence, latest.sequence), 0);
        let diffs: usize = steps.iter().map(|s| s.sequences().count()).sum();
        assert!(diffs < 10 + 2 * 24 + 2 * 60, "{diffs} diffs");
        // The latest state of each stream, and the state files around where the plan changes streams.
        let files: usize = streams.iter().map(|s| s.files.borrow().len()).sum();
        let reads: usize = streams.iter().map(|s| s.reads.get()).sum();
        assert!(files <= 150, "{files} state files, {reads} reads");

        // Up to a time: no diff ends after it.
        let to = latest.timestamp_ms - 3 * DAY - 30 * MINUTE;
        let steps = plan(&dyns(&streams), after(2, from), Some(to)).unwrap();
        let Located::At(end) = streams[2].expected(to) else {
            unreachable!()
        };
        assert_eq!(planet.check(&steps, from.sequence, end.sequence), 0);

        // From where the last step ended, in any stream: nothing more to read.
        let last = steps.last().unwrap();
        let more = plan(&dyns(&streams), after(last.stream, last.to), Some(to)).unwrap();
        assert_eq!(more, []);
    }

    #[test]
    fn every_start_is_followed_by_every_change() {
        let mut rng = StdRng::seed_from_u64(5);
        let mut times = minutes(&mut rng, 20);
        // Some minutes older than the minute before, by up to half of `LATE_MS`, no two in a row.
        for _ in 0..300 {
            let m = rng.random_range(1..times.len() / 3) * 3;
            times[m] = times[m - 1] - rng.random_range(0..LATE_MS / 2);
        }
        let planet = Planet::new(times);
        let streams = planet.streams();
        let latest = streams[2].latest().unwrap().sequence;
        for _ in 0..300 {
            let stream = rng.random_range(0..3);
            let sequence = rng.random_range(1..=streams[stream].last);
            let state = streams[stream].state(sequence);
            let steps = plan(&dyns(&streams), after(stream, state), None).unwrap();
            let twice = planet.check(&steps, planet.end(stream, sequence), latest);
            assert_eq!(twice, 0, "{steps:?}");
        }
    }

    #[test]
    fn after_an_outage_the_steps_down_start_early_enough() {
        let mut rng = StdRng::seed_from_u64(6);
        let mut times = minutes(&mut rng, 20);
        // Outages of 70 minutes to 2 days, after which the minute stream resumes with one diff of all that was missed.
        for _ in 0..12 {
            let at = rng.random_range(1000..times.len() - 4000);
            let gap = rng.random_range(70..2 * 1440);
            times.drain(at..at + gap);
        }
        let planet = Planet::new(times);
        let streams = planet.streams();
        let latest = streams[2].latest().unwrap().sequence;
        let mut overlap = 0;
        for _ in 0..300 {
            let stream = rng.random_range(0..3);
            let sequence = rng.random_range(1..=streams[stream].last);
            let state = streams[stream].state(sequence);
            let steps = plan(&dyns(&streams), after(stream, state), None).unwrap();
            overlap += planet.check(&steps, planet.end(stream, sequence), latest);
        }
        // Only where a plan steps up into an hour cut early, which its state cannot show.
        assert!(overlap < 300, "{overlap} minute diffs read twice");
    }

    #[test]
    fn snapshots_and_times_are_followed_by_every_change_after_them() {
        // Changes commit through each minute, made up to `LATE_MS` before they commit; a diff is cut at each minute
        // and its state is the newest timestamp in it. A snapshot taken at `taken` has all committed before it.
        let mut rng = StdRng::seed_from_u64(7);
        let changes: Vec<(i64, i64)> = (0..200 * 1440)
            .map(|_| {
                let commit = rng.random_range(MINUTE..2 * DAY);
                let late = if rng.random_range(0..100) == 0 {
                    rng.random_range(0..LATE_MS)
                } else {
                    rng.random_range(0..5 * SECOND)
                };
                (commit, commit - late)
            })
            .collect();
        let cut = |commit: i64| commit.div_euclid(MINUTE) + 1;
        let mut times = vec![0; 2 * 1440 + 1];
        for &(commit, made) in &changes {
            let diff = &mut times[index(u64::try_from(cut(commit)).unwrap())];
            *diff = (*diff).max(made);
        }
        let minutes = Stream::of(0, Rc::from(times));
        for _ in 0..200 {
            let taken = rng.random_range(HOUR..2 * DAY - HOUR);
            let newest = changes
                .iter()
                .filter(|c| c.0 <= taken)
                .map(|c| c.1)
                .max()
                .unwrap();
            let steps = plan(
                &[&minutes as &dyn StateSource],
                Start::Snapshot {
                    timestamp_ms: newest,
                },
                None,
            )
            .unwrap();
            let first = steps[0].from.sequence + 1;
            let first_missing = changes
                .iter()
                .filter(|c| c.0 > taken)
                .map(|c| cut(c.0))
                .min()
                .unwrap();
            assert!(
                ms(first) <= first_missing,
                "taken at {taken}: starts at {first}"
            );
            assert!(
                first_missing - ms(first) <= LATE_MS / MINUTE + 2,
                "taken at {taken}: starts at {first}"
            );
        }
        // After a time: every change made after it, from the first diff whose state is after it.
        for _ in 0..200 {
            let t = rng.random_range(HOUR..2 * DAY - HOUR);
            let steps = plan(
                &[&minutes as &dyn StateSource],
                Start::Time { timestamp_ms: t },
                None,
            )
            .unwrap();
            let from = steps[0].from;
            let first_made_after = changes
                .iter()
                .filter(|c| c.1 > t)
                .map(|c| cut(c.0))
                .min()
                .unwrap();
            assert!(
                ms(from.sequence) < first_made_after,
                "time {t}: after {from:?}"
            );
            assert!(from.timestamp_ms <= t);
            assert!(
                minutes.state(from.sequence + 1).timestamp_ms > t,
                "time {t}"
            );
        }
        assert!(matches!(
            plan(
                &[&minutes as &dyn StateSource],
                Start::Snapshot { timestamp_ms: 0 },
                None
            ),
            Err(OscError::TooOld { .. })
        ));
    }

    #[test]
    fn coarse_streams_are_used_where_they_have_whole_diffs() {
        // Days end at day 38, and hours start at day 40: minutes up to day 36, two days, minutes up to day 40, hours
        // up to day 50, and minutes after.
        let streams = [
            Stream::every(30..=38, |d| ms(d) * DAY),
            Stream::every(40 * 24..=50 * 24, |h| ms(h) * HOUR),
            Stream::every(30 * 1440..=50 * 1440 + 125, |m| ms(m) * MINUTE),
        ];
        let from = streams[2].state(35 * 1440 + 120);
        let steps = plan(&dyns(&streams), after(2, from), None).unwrap();
        let shape: Vec<_> = steps
            .iter()
            .map(|s| (s.stream, s.sequences().count()))
            .collect();
        assert_eq!(
            shape,
            [(2, 22 * 60), (0, 2), (2, 2 * 1440), (1, 240), (2, 125)]
        );
        // The finest stream must reach back.
        assert!(matches!(
            plan(&dyns(&streams), after(0, streams[0].state(30)), None),
            Err(OscError::TooOld { .. })
        ));
        // Caught up already: nothing to read.
        let latest = streams[2].latest().unwrap();
        assert_eq!(plan(&dyns(&streams), after(2, latest), None).unwrap(), []);
        assert_eq!(plan(&[], after(0, latest), None).unwrap(), []);
    }
}
