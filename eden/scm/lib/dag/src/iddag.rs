/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::{self};
use std::ops::Deref;
#[cfg(any(test, feature = "indexedlog-backend"))]
use std::path::Path;

use indexmap::set::IndexSet;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::trace;

use crate::errors::bug;
use crate::errors::NotFoundError;
use crate::id::Group;
use crate::id::Id;
use crate::iddagstore::IdDagStore;
use crate::iddagstore::InProcessStore;
#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::iddagstore::IndexedLogStore;
use crate::ops::Persist;
#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::ops::TryClone;
use crate::segment::FlatSegment;
use crate::segment::PreparedFlatSegments;
use crate::segment::Segment;
use crate::segment::SegmentFlags;
use crate::spanset;
use crate::Error::Programming;
use crate::IdSet;
use crate::IdSpan;
use crate::Level;
use crate::Result;
use crate::VerLink;

/// Structure to store a DAG of integers, with indexes to speed up ancestry queries.
///
/// A segment is defined as `(level: int, low: int, high: int, parents: [int])` on
/// a topo-sorted integer DAG. It covers all integers in `low..=high` range, and
/// must satisfy:
/// - `high` is the *only* head in the sub DAG covered by the segment.
/// - `parents` do not have entries within `low..=high` range.
/// - If `level` is 0, for any integer `x` in `low+1..=high` range, `x`'s parents
///   must be `x - 1`.
///
/// See `slides/201904-segmented-changelog/segmented-changelog.pdf` for pretty
/// graphs about how segments help with ancestry queries.
///
/// [`IdDag`] is often used together with [`IdMap`] to allow customized names
/// on vertexes. The [`NameDag`] type provides an easy-to-use interface to
/// keep [`IdDag`] and [`IdMap`] in sync.
#[derive(Clone, Serialize, Deserialize)]
pub struct IdDag<Store> {
    store: Store,
    #[serde(skip, default = "default_seg_size")]
    new_seg_size: usize,
    #[serde(skip, default = "VerLink::new")]
    version: VerLink,
}

/// See benches/segment_sizes.rs (D16660078) for this choice.
const DEFAULT_SEG_SIZE: usize = 16;

/// Maximum meaningful level. 4 is chosen because it is good enough
/// for an existing large repo (level 5 is not built because it
/// cannot merge level 4 segments).
const MAX_MEANINGFUL_LEVEL: Level = 4;

#[cfg(any(test, feature = "indexedlog-backend"))]
impl IdDag<IndexedLogStore> {
    /// Open [`IdDag`] at the given directory. Create it on demand.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let store = IndexedLogStore::open(path)?;
        Self::open_from_store(store)
    }
}

impl<S> IdDag<S> {
    /// Set the maximum size of a new high-level segment.
    ///
    /// This does not affect existing segments.
    ///
    /// This might help performance a bit for certain rare types of DAGs.
    /// The default value is Usually good enough.
    pub fn set_new_segment_size(&mut self, size: usize) {
        self.new_seg_size = size.max(2);
    }

    /// Get the segment size used for building new high-level segments.
    pub(crate) fn get_new_segment_size(&self) -> usize {
        self.new_seg_size
    }
}

#[cfg(any(test, feature = "indexedlog-backend"))]
impl TryClone for IdDag<IndexedLogStore> {
    /// Attempt to clone the `IdDag`.
    fn try_clone(&self) -> Result<Self> {
        let store = self.store.try_clone()?;
        Ok(Self {
            store,
            new_seg_size: self.new_seg_size,
            version: self.version.clone(),
        })
    }
}

impl IdDag<InProcessStore> {
    /// Instantiate an [`IdDag`] that stores all it's data in process. Useful for scenarios that
    /// do not require data persistance.
    pub fn new_in_process() -> Self {
        let store = InProcessStore::new();
        Self {
            store,
            new_seg_size: DEFAULT_SEG_SIZE,
            version: VerLink::new(),
        }
    }
}

impl<Store: IdDagStore> IdDag<Store> {
    pub(crate) fn open_from_store(store: Store) -> Result<Self> {
        let dag = Self {
            store,
            new_seg_size: DEFAULT_SEG_SIZE, // see D16660078 for this default setting
            version: VerLink::new(),
        };
        Ok(dag)
    }
}

impl<Store: IdDagStore> IdDag<Store> {
    /// Add a new segment.
    ///
    /// For simplicity, it does not check if the new segment overlaps with
    /// an existing segment (which is a logic error). Those checks can be
    /// offline.
    pub(crate) fn insert(
        &mut self,
        flags: SegmentFlags,
        level: Level,
        low: Id,
        high: Id,
        parents: &[Id],
    ) -> Result<()> {
        self.version.bump();
        self.store.insert(flags, level, low, high, parents)
    }

    /// Returns whether the iddag contains segments for the given `id`.
    pub fn contains_id(&self, id: Id) -> Result<bool> {
        let group = id.group();
        let level = 0;
        Ok(self.next_free_id(level, group)? > id)
    }

    pub(crate) fn version(&self) -> &VerLink {
        &self.version
    }
}

// Build segments.
impl<Store: IdDagStore> IdDag<Store> {
    /// Make sure the [`IdDag`] contains the given id (and all ids smaller than
    /// `high`) by building up segments on demand.
    ///
    /// `get_parents` describes the DAG. Its input and output are `Id`s.
    ///
    /// This is often used together with [`crate::idmap::IdMap`].
    pub fn build_segments_volatile<F>(&mut self, high: Id, get_parents: &F) -> Result<usize>
    where
        F: Fn(Id) -> Result<Vec<Id>>,
    {
        let mut count = 0;
        count += self.build_flat_segments(high, get_parents, 0)?;
        if self.next_free_id(0, high.group())? <= high {
            return bug("internal error: flat segments are not built as expected");
        }
        count += self.build_all_high_level_segments(Level::MAX)?;
        Ok(count)
    }

    /// Similar to `build_segments_volatile`, but takes `PreparedFlatSegments` instead
    /// of `get_parents`.
    pub fn build_segments_volatile_from_prepared_flat_segments(
        &mut self,
        outcome: &PreparedFlatSegments,
    ) -> Result<usize> {
        let mut count = self.build_flat_segments_from_prepared_flat_segments(outcome)?;
        count += self.build_all_high_level_segments(Level::MAX)?;
        Ok(count)
    }

    /// Build flat segments using the outcome from `add_head`.
    /// This is not public because it does not keep high-level segments in sync.
    fn build_flat_segments_from_prepared_flat_segments(
        &mut self,
        outcome: &PreparedFlatSegments,
    ) -> Result<usize> {
        if outcome.segments.is_empty() {
            return Ok(0);
        }

        // TODO: Modify the last segment if it can concat the first new segment.

        let mut head_ids: HashSet<Id> = self.heads(self.master_group()?)?.iter().collect();
        let mut get_flags = |parents: &[Id], head: Id| {
            let mut flags = SegmentFlags::empty();
            if parents.is_empty() {
                flags |= SegmentFlags::HAS_ROOT
            }
            if head.group() == Group::MASTER {
                for p in parents.iter() {
                    head_ids.remove(p);
                }
                if head_ids.is_empty() {
                    flags |= SegmentFlags::ONLY_HEAD;
                }
                head_ids.insert(head);
            }
            flags
        };
        for seg in &outcome.segments {
            // `next_free_id` has cost. Therefore the check is only on debug build.
            debug_assert_eq!(
                seg.low,
                self.next_free_id(0, seg.low.group())?,
                "outcome low id mismatch"
            );

            let flags = get_flags(&seg.parents, seg.high);
            tracing::trace!(
                "inserting flat segment {}..={} {:?} {:?}",
                seg.low,
                seg.high,
                &seg.parents,
                &flags
            );
            self.insert(flags, 0, seg.low, seg.high, &seg.parents)?;
        }
        Ok(outcome.segments.len())
    }

    /// Incrementally build flat (level 0) segments towards `high` (inclusive).
    ///
    /// `get_parents` describes the DAG. Its input and output are `Id`s.
    ///
    /// `last_threshold` decides the minimal size for the last incomplete flat
    /// segment. Setting it to 0 will makes sure flat segments cover the given
    /// `high - 1`, with the downside of increasing fragmentation.  Setting it
    /// to a larger value will reduce fragmentation, with the downside of
    /// [`IdDag`] covers less ids.
    ///
    /// Return number of segments inserted.
    fn build_flat_segments<F>(
        &mut self,
        high: Id,
        get_parents: &F,
        last_threshold: u64,
    ) -> Result<usize>
    where
        F: Fn(Id) -> Result<Vec<Id>>,
    {
        let group = high.group();
        let low = self.next_free_id(0, group)?;
        let mut current_low = None;
        let mut current_parents = Vec::new();
        let mut insert_count = 0;
        let mut head_ids: HashSet<Id> = if group == Group::MASTER && low > Id::MIN {
            self.heads((Id::MIN..=(low - 1)).into())?.iter().collect()
        } else {
            Default::default()
        };
        let mut get_flags = |parents: &Vec<Id>, head: Id| {
            let mut flags = SegmentFlags::empty();
            if parents.is_empty() {
                flags |= SegmentFlags::HAS_ROOT
            }
            if group == Group::MASTER {
                head_ids = &head_ids - &parents.iter().cloned().collect();
                if head_ids.is_empty() {
                    flags |= SegmentFlags::ONLY_HEAD;
                }
                head_ids.insert(head);
            }
            flags
        };
        for id in low.to(high) {
            let parents = get_parents(id)?;
            if parents.len() != 1 || parents[0] + 1 != id || current_low.is_none() {
                // Must start a new segment.
                if let Some(low) = current_low {
                    debug_assert!(id > Id::MIN);
                    let flags = get_flags(&current_parents, id - 1);
                    self.insert(flags, 0, low, id - 1, &current_parents)?;
                    insert_count += 1;
                }
                current_parents = parents;
                current_low = Some(id);
            }
        }

        // For the last flat segment, only build it if its length satisfies the threshold.
        if let Some(low) = current_low {
            if low + last_threshold <= high {
                let flags = get_flags(&current_parents, high);
                self.insert(flags, 0, low, high, &current_parents)?;
                insert_count += 1;
            }
        }

        Ok(insert_count)
    }

    /// Incrementally build high level segments at the given `level`.
    ///
    /// The new, high level segments are built on top of the lower level
    /// (`level - 1`) segments. Each high level segment covers at most `size`
    /// `level - 1` segments.
    ///
    /// The last segment per level per group is dropped because it's likely to
    /// be incomplete. This helps reduce fragmentation, and also allows the last
    /// flat segment per group to be mutable, because it will not be included
    /// in Level 1 segments.
    ///
    /// Return number of segments inserted.
    fn build_high_level_segments(&mut self, level: Level) -> Result<usize> {
        if level == 0 {
            // Do nothing. Level 0 is not considered high level.
            return Ok(0);
        }
        let size = self.new_seg_size;

        let mut insert_count = 0;
        let mut new_segments_per_group = Vec::new();
        let mut lower_segments_len = 0;
        for &group in Group::ALL.iter() {
            // `get_parents` is on the previous level of segments.
            let get_parents = |head: Id| -> Result<Vec<Id>> {
                if let Some(seg) = self.find_segment_by_head_and_level(head, level - 1)? {
                    seg.parents()
                } else {
                    bug("get_parents called with wrong head in build_high_level_segments")
                }
            };

            let new_segments = {
                let low = self.next_free_id(level, group)?;

                // Find all segments on the previous level that haven't been built.
                let segments: Vec<_> = self.next_segments(low, level - 1)?;
                lower_segments_len += segments.len();

                // Sanity check: They should be sorted and connected.
                for i in 1..segments.len() {
                    if segments[i - 1].high()? + 1 != segments[i].span()?.low {
                        let msg = format!(
                            "level {} segments {:?} are not sorted or connected!",
                            level,
                            &segments[i - 1..=i]
                        );
                        return bug(msg);
                    }
                }

                // Build the graph from the first head. `low_idx` is the
                // index of `segments` (level - 1).

                // find_segment scans low level segments (segments[low_idx..]),
                // merges them on the fly, and returns a high-level segment:
                // (new_idx, low, high, parents, has_root).
                // new_idx + 1 is the next low_idx that should be passed to find_segment
                // to calculate the next high-level segment.
                let find_segment = |low_idx: usize| -> Result<_> {
                    let segment_low = segments[low_idx].span()?.low;
                    let mut heads = BTreeSet::new();
                    let mut parents = IndexSet::new();
                    let mut candidate = None;
                    let mut has_root = false;
                    for i in low_idx..segments.len().min(low_idx + size) {
                        // [--------------------------] level (to build)
                        // [------] [------] [--------] level - 1 (lower level)
                        // ^        ^
                        // |        segments[i].low
                        // segment_low
                        let span = segments[i].span()?;
                        let head = span.high;
                        if !has_root && segments[i].has_root()? {
                            has_root = true;
                        }
                        heads.insert(head);
                        let direct_parents = get_parents(head)?;
                        for p in &direct_parents {
                            if *p >= span.low {
                                return bug(format!(
                                    "invalid lv{} segment: {:?} (parent >= low)",
                                    level - 1,
                                    &segments[i]
                                ));
                            }
                            if *p < segment_low {
                                // No need to remove p from heads, since it cannot be a head.
                                parents.insert(*p);
                            } else {
                                heads.remove(p);
                            }
                        }
                        if heads.len() == 1 {
                            candidate = Some((i, segment_low, head, parents.len(), has_root));
                        }
                    }
                    // There must be at least one valid high-level segment,
                    // because `segments[low_idx]` is such a high-level segment.
                    let (new_idx, low, high, parent_count, has_root) = candidate.unwrap();
                    let parents = parents.into_iter().take(parent_count).collect::<Vec<Id>>();
                    Ok((new_idx, low, high, parents, has_root))
                };

                let mut idx = 0;
                let mut new_segments = Vec::new();
                while idx < segments.len() {
                    let segment_info = find_segment(idx)?;
                    idx = segment_info.0 + 1;
                    new_segments.push(segment_info);
                }

                new_segments
            };

            new_segments_per_group.push(new_segments);
        }

        // No point to introduce new levels if it has the same segment count
        // as the lower level.
        if level > self.max_level()?
            && new_segments_per_group
                .iter()
                .fold(0, |acc, s| acc + s.len())
                >= lower_segments_len
        {
            return Ok(0);
        }

        for mut new_segments in new_segments_per_group {
            // Drop the last segment. It could be incomplete.
            new_segments.pop();

            insert_count += new_segments.len();

            for (_, low, high, parents, has_root) in new_segments {
                let flags = if has_root {
                    SegmentFlags::HAS_ROOT
                } else {
                    SegmentFlags::empty()
                };
                tracing::trace!(
                    "inserting lv{} segment {}..{} {:?} {:?}",
                    level,
                    low,
                    high,
                    &parents,
                    &flags
                );
                self.insert(flags, level, low, high, &parents)?;
            }
        }

        Ok(insert_count)
    }

    /// Build high level segments using default setup.
    ///
    /// Return number of segments inserted.
    fn build_all_high_level_segments(&mut self, max_level: Level) -> Result<usize> {
        let mut total = 0;
        let max_level = max_level.min(MAX_MEANINGFUL_LEVEL);
        for level in 1..=max_level {
            let count = self.build_high_level_segments(level)?;
            tracing::debug!("new lv{} segments: {}", level, count);
            if count == 0 {
                break;
            }
            total += count;
        }
        Ok(total)
    }
}

impl<Store: IdDagStore> IdDag<Store> {
    /// Returns the [`FlatSegment`] entries that are used by this [`IdDag`].
    pub fn flat_segments(&self, group: Group) -> Result<PreparedFlatSegments> {
        let segments = self.flat_segments_range(group.min_id(), group.max_id())?;
        Ok(PreparedFlatSegments { segments })
    }

    /// Return all flat segments that overlap with range (and potentially cover
    /// larger range than supplied).
    fn flat_segments_range(&self, min: Id, max_incl: Id) -> Result<Vec<FlatSegment>> {
        let level = 0;
        let mut segments = Vec::new();
        for sr in self.iter_segments_ascending(min, level)? {
            let segment = sr?;
            let span = segment.span()?;
            if span.low > max_incl {
                break;
            }
            let fs = FlatSegment {
                low: span.low,
                high: span.high,
                parents: segment.parents()?,
            };
            segments.push(fs);
        }
        Ok(segments)
    }

    /// Extract flat segments that cover the given `set` exactly.
    pub fn idset_to_flat_segments(&self, set: IdSet) -> Result<PreparedFlatSegments> {
        let mut segments = Vec::new();

        let (min, max) = if let (Some(min), Some(max)) = (set.min(), set.max()) {
            (min, max)
        } else {
            return Ok(PreparedFlatSegments { segments });
        };
        let segs = self.flat_segments_range(min, max)?;
        let seg_iter = segs.into_iter().rev();

        let push = |seg: FlatSegment| segments.push(seg);
        let span_iter = set.as_spans().iter().cloned();
        spanset::intersect_iter(seg_iter, span_iter, push);

        segments.reverse();

        Ok(PreparedFlatSegments { segments })
    }
}

// User-facing DAG-related algorithms.
pub trait IdDagAlgorithm: IdDagStore {
    /// Return a [`IdSet`] that covers all ids stored in this [`IdDag`].
    fn all(&self) -> Result<IdSet> {
        self.all_ids_in_groups(&Group::ALL)
    }

    /// Return a [`IdSet`] that covers all ids stored in the master group.
    fn master_group(&self) -> Result<IdSet> {
        self.all_ids_in_groups(&[Group::MASTER])
    }

    /// Calculate all ancestors reachable from any id from the given set.
    ///
    /// ```plain,ignore
    /// union(ancestors(i) for i in set)
    /// ```
    fn ancestors(&self, mut set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::ancestors", "{}", msg());
        }
        debug!(target: "dag::algo::ancestors", "ancestors({:?})", &set);
        if set.count() > 2 {
            // Try to (greatly) reduce the size of the `set` to make calculation cheaper.
            set = self.heads_ancestors(set)?;
            trace(&|| format!("simplified to {:?}", &set));
        }
        let mut result = IdSet::empty();
        let mut to_visit: BinaryHeap<_> = set.iter().collect();
        let max_level = self.max_level()?;
        'outer: while let Some(id) = to_visit.pop() {
            if result.contains(id) {
                // If `id` is in `result`, then `ancestors(id)` are all in `result`.
                continue;
            }
            trace(&|| format!(" lookup {:?}", id));
            let flat_seg = self.find_flat_segment_including_id(id)?;
            if let Some(ref s) = flat_seg {
                if s.only_head()? {
                    // Fast path.
                    trace(&|| format!(" push ..={:?} (only head fast path)", id));
                    result.push_span((Id::MIN..=id).into());
                    break 'outer;
                }
            }
            for level in (1..=max_level).rev() {
                let seg = self.find_segment_by_head_and_level(id, level)?;
                if let Some(seg) = seg {
                    let span = seg.span()?.into();
                    trace(&|| format!(" push lv{} {:?}", level, &span));
                    result.push_span(span);
                    let parents = seg.parents()?;
                    trace(&|| format!(" follow parents {:?}", &parents));
                    for parent in parents {
                        to_visit.push(parent);
                    }
                    continue 'outer;
                }
            }
            if let Some(seg) = flat_seg {
                let span = (seg.span()?.low..=id).into();
                trace(&|| format!(" push lv0 {:?}", &span));
                result.push_span(span);
                let parents = seg.parents()?;
                trace(&|| format!(" follow parents {:?}", &parents));
                for parent in parents {
                    to_visit.push(parent);
                }
            } else {
                return bug("flat segments are expected to cover everything but they are not");
            }
        }

        trace(&|| format!(" result: {:?}", &result));

        Ok(result)
    }

    /// Like `ancestors` but follows only the first parents.
    fn first_ancestors(&self, set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::first_ancestors", "{}", msg());
        }
        debug!(target: "dag::algo::first_ancestors", "first_ancestors({:?})", &set);
        let mut result = IdSet::empty();
        let mut to_visit: BinaryHeap<_> = set.iter().collect();
        // Lookup flat segments to figure out the first ancestors.
        while let Some(id) = to_visit.pop() {
            if result.contains(id) {
                // If `id` is in `result`, then `ancestors(id)` are all in `result`.
                continue;
            }
            trace(&|| format!(" visit {:?}", &id));
            let flat_seg = self.find_flat_segment_including_id(id)?;
            if let Some(ref seg) = flat_seg {
                let span = seg.span()?;
                result.push_span((span.low..=id).into());
                trace(&|| format!(" push {:?}..={:?}", span.low, id));
                if let Some(&p) = seg.parents()?.get(0) {
                    to_visit.push(p);
                }
            }
        }
        trace(&|| format!(" result: {:?}", &result));
        Ok(result)
    }

    /// Calculate merges within the given set.
    fn merges(&self, set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::merges", "{}", msg());
        }
        debug!(target: "dag::algo::merges", "merges({:?})", &set);

        let mut result = IdSet::empty();

        // Check overlapped flat segments. By definition, merges can only be the
        // "low"s of flat segments.

        // Process the given span overlapped with the segment.
        // Return the next "high" id for segment lookup.
        // Return None if there is no segment to check for the given span.
        let mut process_seg = |span: &IdSpan, seg: Segment| -> Result<Option<Id>> {
            trace(&|| format!(" process {:?} seg {:?}", &span, &seg));
            let seg_span = seg.span()?;
            let low = seg_span.low;
            if low < span.low {
                return Ok(None);
            }
            if seg.parent_count()? >= 2 {
                // span.low <= low <= high <= span.high
                debug_assert!(set.contains(low));
                trace(&|| format!(" push merge {:?}", &low));
                result.push_span(low.into());
            }
            if seg_span.low > Id(0) {
                Ok(Some(seg_span.low - 1))
            } else {
                Ok(None)
            }
        };

        for span in set.as_spans() {
            // Cannot use iter_segments_descending, since it skips overlapping
            // segments (seg.high > span.high and seg.low > span.low). Use
            // find_flat_segment_including_id to find the first overlapping
            // segment, then use iter_segments_descending to handle a large
            // span (ex. all()) efficiently.
            let high = match self.find_flat_segment_including_id(span.high)? {
                None => continue,
                Some(seg) => match process_seg(span, seg)? {
                    None => continue,
                    Some(id) => id,
                },
            };
            'iter_seg: for seg in self.iter_segments_descending(high, 0)? {
                let seg = seg?;
                match process_seg(span, seg)? {
                    None => break 'iter_seg,
                    Some(_) => {}
                }
            }
        }

        trace(&|| format!(" result: {:?}", &result));

        Ok(result)
    }

    /// Calculate parents of the given set.
    ///
    /// Note: [`IdSet`] does not preserve order. Use [`IdDag::parent_ids`] if
    /// order is needed.
    fn parents(&self, mut set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::parents", "{}", msg());
        }
        debug!(target: "dag::algo::parents", "parents({:?})", &set);

        let mut result = IdSet::empty();
        let max_level = self.max_level()?;

        'outer: while let Some(head) = set.max() {
            trace(&|| format!("check head {:?}", head));
            // For high-level segments. If the set covers the entire segment, then
            // the parents is (the segment - its head + its parents).
            for level in (1..=max_level).rev() {
                if let Some(seg) = self.find_segment_by_head_and_level(head, level)? {
                    let seg_span = seg.span()?;
                    if set.contains(seg_span) {
                        let seg_set = IdSet::from(seg_span);
                        let mut parent_set = seg_set.difference(&head.into());
                        parent_set.push_set(&IdSet::from_spans(seg.parents()?));
                        set = set.difference(&seg_set);
                        result = result.union(&parent_set);
                        trace(&|| format!(" push lv{} {:?}", level, &parent_set));
                        continue 'outer;
                    }
                }
            }

            // A flat segment contains information to calculate
            // parents(subset of the segment).
            let seg = match self.find_flat_segment_including_id(head)? {
                Some(seg) => seg,
                None => return head.not_found(),
            };
            let seg_span = seg.span()?;
            let seg_low = seg_span.low;
            let seg_set: IdSet = seg_span.into();
            let seg_set = seg_set.intersection(&set);

            // Get parents for a linear set (ex. parent(i) is (i - 1)).
            fn parents_linear(set: &IdSet) -> IdSet {
                debug_assert!(!set.contains(Id::MIN));
                IdSet::from_sorted_spans(set.as_spans().iter().map(|s| s.low - 1..=s.high - 1))
            }

            let parent_set = if seg_set.contains(seg_low) {
                let mut parent_set = parents_linear(&seg_set.difference(&IdSet::from(seg_low)));
                parent_set.push_set(&IdSet::from_spans(seg.parents()?));
                parent_set
            } else {
                parents_linear(&seg_set)
            };

            set = set.difference(&seg_set);
            trace(&|| format!(" push lv0 {:?}", &parent_set));
            result = result.union(&parent_set);
        }

        trace(&|| format!(" result: {:?}", &result));

        Ok(result)
    }

    /// Get parents of a single `id`. Preserve the order.
    fn parent_ids(&self, id: Id) -> Result<Vec<Id>> {
        let seg = match self.find_flat_segment_including_id(id)? {
            Some(seg) => seg,
            None => return id.not_found(),
        };
        let span = seg.span()?;
        if id == span.low {
            Ok(seg.parents()?)
        } else {
            Ok(vec![id - 1])
        }
    }

    /// Calculate the n-th first ancestor. If `n` is 0, return `id` unchanged.
    /// If `n` is 1, return the first parent of `id`.
    fn first_ancestor_nth(&self, id: Id, n: u64) -> Result<Id> {
        match self.try_first_ancestor_nth(id, n)? {
            None => Err(Programming(format!(
                "{}~{} cannot be resolved - no parents",
                &id, n
            ))),
            Some(id) => Ok(id),
        }
    }

    /// Calculate the n-th first ancestor. If `n` is 0, return `id` unchanged.
    /// If `n` is 1, return the first parent of `id`.
    /// If `n` is too large, exceeding the distance between the root and `id`,
    /// return `None`.
    fn try_first_ancestor_nth(&self, mut id: Id, mut n: u64) -> Result<Option<Id>> {
        // PERF: this can have fast paths from high-level segments if high-level
        // segments have extra information.
        while n > 0 {
            let seg = self
                .find_flat_segment_including_id(id)?
                .ok_or_else(|| id.not_found_error())?;
            // segment: low ... id ... high
            //          \________/
            //            delta
            let low = seg.span()?.low;
            let delta = id.0 - low.0;
            let step = delta.min(n);
            id = id - step;
            n -= step;
            if n > 0 {
                // Follow the first parent.
                id = match seg.parents()?.get(0) {
                    None => return Ok(None),
                    Some(&id) => id,
                };
                n -= 1;
            }
        }
        Ok(Some(id))
    }

    /// Convert an `id` to `x~n` form with the given constraint.
    ///
    /// Return `None` if the conversion can not be done with the constraints.
    fn to_first_ancestor_nth(
        &self,
        id: Id,
        constraint: FirstAncestorConstraint,
    ) -> Result<Option<(Id, u64)>> {
        match constraint {
            FirstAncestorConstraint::None => Ok(Some((id, 0))),
            FirstAncestorConstraint::KnownUniversally { heads } => {
                self.to_first_ancestor_nth_known_universally(id, heads)
            }
        }
    }

    /// See `FirstAncestorConstraint::KnownUniversally`.
    ///
    /// Try to represent `id` as `x~n` (revset notation) form, where `x` must
    /// be in `heads + parents(merge() & ancestors(heads))`.
    ///
    /// Return `None` if `id` is not part of `ancestors(heads)`.
    fn to_first_ancestor_nth_known_universally(
        &self,
        id: Id,
        heads: IdSet,
    ) -> Result<Option<(Id, u64)>> {
        // Do not track errors for the first time. This has lower overhead.
        match self.to_first_ancestor_nth_known_universally_with_errors(id, &heads, None) {
            Ok(v) => Ok(v),
            Err(_) => {
                // Pay additional overhead to get error message.
                self.to_first_ancestor_nth_known_universally_with_errors(
                    id,
                    &heads,
                    Some(Vec::new()),
                )
            }
        }
    }

    /// Internal implementation of `to_first_ancestor_nth_known_universally`.
    ///
    /// If `details` is not `None`, it is used to store extra error messages
    /// with some extra overhead.
    fn to_first_ancestor_nth_known_universally_with_errors(
        &self,
        mut id: Id,
        heads: &IdSet,
        mut details: Option<Vec<String>>,
    ) -> Result<Option<(Id, u64)>> {
        // To figure `x~n` we look at the flat segment containing `id`, and check:
        // - If the flat segment overlaps with heads, then just use the overlapped id as `x`.
        // - Try to find parents of merges within the flat segment (the merges belong to other
        //   "child segment"s). If the merge is an ancestor of `heads`, then the parent of the
        //   merge can be used as `x`, if `n` is not 0.
        // - Otherwise, try to follow connected flat segment (with single parent), convert the
        //   `x~n` question to a `y~(n+m)` question, then start over.
        let mut trace = |msg: &dyn Fn() -> String| {
            if let Some(details) = details.as_mut() {
                details.push(msg().trim().to_string());
            }
            trace!(target: "dag::algo::toxn", "{}", msg());
        };

        let ancestors = self.ancestors(heads.clone())?;
        if !ancestors.contains(id) {
            return Ok(None);
        }

        let mut n = 0;
        debug!(target: "dag::algo::toxn", "resolving {:?}", id);
        let result = 'outer: loop {
            let seg = self
                .find_flat_segment_including_id(id)?
                .ok_or_else(|| id.not_found_error())?;
            let span = seg.span()?;
            let head = span.high;
            trace(&|| format!(" in seg {:?}", &seg));
            // Can we use an `id` from `heads` as `x`?
            let intersected = heads.intersection(&(id..=head).into());
            if !intersected.is_empty() {
                let head = intersected.min().unwrap();
                n += head.0 - id.0;
                trace(&|| format!("  contains head ({:?})", head));
                break 'outer (head, n);
            }
            // Does this segment contain any `x` that is an ancestor of a merge,
            // that can be used as `x`?
            //
            // Note the `x` does not have to be the head of `seg`. For example,
            //
            //      1--2--3--4 (seg, span: 1..=4)
            //          \
            //           5--6  (child_seg, span: 5..=6, parents: [2])
            //
            // During `to_first_ancestor_nth(2, [6])`, `1` needs to be translated
            // to `6~3`, not `4~3`. Needs to lookup parents in the range `1..=4`.
            let mut next_id_n = None;
            let parent_span = span.low.max(id)..=span.high;
            for entry in self.iter_master_flat_segments_with_parent_span(parent_span.into())? {
                let (parent_id, child_seg) = entry?;
                trace(&|| format!("  {:?} has child seg ({:?})", parent_id, &child_seg));
                let child_low = child_seg.low()?;
                if !ancestors.contains(child_low) {
                    // `child_low` is outside `ancestors(heads)`, cannot use it
                    // or its parents as references.
                    trace(&|| "   child seg out of range".to_string());
                    continue;
                }

                if child_seg.parent_count()? > 1 {
                    // `child_low` is a merge included in `ancestors`, and
                    // `parent_id` is a parent of the merge. Therefore
                    // `parent_id` might be used as `x`.
                    let next_n = n + parent_id.0 - id.0;
                    if next_n > 0 {
                        break 'outer (parent_id, next_n);
                    }

                    // Fragmented linear segments. Convert id~n to next_id~next_n.
                    let child_parents = child_seg.parents()?;
                    match child_parents.get(0) {
                        None => {
                            return bug(format!(
                                "segment {:?} should have parent {:?}",
                                &child_seg, &parent_id
                            ));
                        }
                        Some(p) => {
                            if &parent_id != p {
                                // Not the first parent. Do not follow it.
                                trace(&|| {
                                    format!(
                                        "   child seg cannot be followed ({:?} is not p1)",
                                        &parent_id
                                    )
                                });
                                continue;
                            }
                        }
                    }
                }

                debug_assert!(ancestors.contains(child_low));
                let next_id = child_low;
                let next_n = n + 1 + parent_id.0 - id.0;
                trace(&|| format!("  follow {:?}~{}", next_id, next_n));
                next_id_n = Some((next_id, next_n));
                break;
            }
            match next_id_n {
                // This should not happen if indexes and segments are legit.
                None => {
                    let mut message = format!(
                        concat!(
                            "cannot convert {} to x~n form (x must be in ",
                            "`H + parents(ancestors(H) & merge())` where H = {:?})",
                        ),
                        id, &heads,
                    );
                    if let Some(details) = details {
                        if !details.is_empty() {
                            message += &format!(" (trace: {})", details.join(", "));
                        }
                    }
                    return Err(Programming(message));
                }
                Some((next_id, next_n)) => {
                    id = next_id;
                    n = next_n;
                }
            }
        };
        trace!(target: "dag::algo::toxn", " found: {:?}", &result);
        Ok(Some(result))
    }

    /// Calculate heads of the given set.
    fn heads(&self, set: IdSet) -> Result<IdSet> {
        Ok(set.difference(&self.parents(set.clone())?))
    }

    /// Calculate children for a single `Id`.
    fn children_id(&self, id: Id) -> Result<IdSet> {
        let mut result = BTreeSet::new();
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::children_id", "{}", msg());
        }
        debug!(target: "dag::algo::children_id", "children_id({:?})", id);
        for seg in self.iter_flat_segments_with_parent(id)? {
            let seg = seg?;
            let child_id = seg.low()?;
            trace(&|| format!(" push {:?} via parent index", child_id));
            result.insert(child_id);
        }
        if let Some(seg) = self.find_flat_segment_including_id(id)? {
            let span = seg.span()?;
            if span.high != id {
                let child_id = id + 1;
                trace(&|| format!(" push {:?} via flat segment definition", child_id));
                result.insert(child_id);
            }
        }
        let result = IdSet::from_sorted_spans(result.into_iter().rev());
        trace(&|| format!(" result: {:?}", &result));
        Ok(result)
    }

    /// Calculate children of the given set.
    fn children(&self, set: IdSet) -> Result<IdSet> {
        if set.count() < 5 {
            let result =
                set.iter()
                    .fold(Ok(IdSet::empty()), |acc: Result<IdSet>, id| match acc {
                        Ok(acc) => Ok(acc.union(&self.children_id(id)?)),
                        Err(err) => Err(err),
                    })?;
            #[cfg(test)]
            {
                let result_set = self.children_set(set)?;
                assert_eq!(result.as_spans(), result_set.as_spans());
            }
            Ok(result)
        } else {
            self.children_set(set)
        }
    }

    fn children_set(&self, set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::children", "{}", msg());
        }
        debug!(target: "dag::algo::children", "children({:?})", &set);

        // The algorithm works as follows:
        // - Iterate through level N segments [1].
        // - Considering a level N segment S:
        //   Could we take the entire S?
        //     - If `set` covers `S - S.head + S.parents`, then yes, take S
        //       and continue with the next level N segment.
        //   Could we ignore the entire S and check the next level N segment?
        //     - If (S + S.parents) do not overlap with `set`, then yes, skip.
        //   No fast paths. Is S a flat segment?
        //     - No:  Iterate through level N-1 segments covered by S,
        //            recursively (goto [1]).
        //     - Yes: Figure out children in the flat segment.
        //            Push them to the result.

        struct Context<'a, Store: ?Sized> {
            this: &'a Store,
            set: IdSet,
            // Lower bound based on the input set.
            result_lower_bound: Id,
            result: IdSet,
        }

        fn visit_segments<S: IdDagStore + ?Sized>(
            ctx: &mut Context<S>,
            mut range: IdSpan,
            level: Level,
        ) -> Result<()> {
            trace(&|| format!("visit range {:?} lv{}", &range, level));
            for seg in ctx.this.iter_segments_descending(range.high, level)? {
                let seg = seg?;
                let span = seg.span()?;
                trace(&|| format!(" seg {:?}", &seg));
                // `range` is all valid. If a high-level segment misses it, try
                // a lower level one.
                if span.high < range.high {
                    let low_id = (span.high + 1).max(range.low);
                    if low_id > range.high {
                        return Ok(());
                    }
                    let missing_range = IdSpan::from(low_id..=range.high);
                    if level > 0 {
                        trace(&|| {
                            format!("  visit missing range at lower level: {:?}", &missing_range)
                        });
                        visit_segments(ctx, missing_range, level - 1)?;
                    } else {
                        return bug(format!(
                            "flat segments should have covered: {:?} returned by all() (range: {:?})",
                            missing_range, range,
                        ));
                    }
                }

                // Update range.high for the next iteration.
                range.high = span.low.max(Id(1)) - 1;

                // Stop iteration?
                if span.high < range.low || span.high < ctx.result_lower_bound {
                    break;
                }

                let parents = seg.parents()?;

                // Count of parents overlapping with `set`.
                let overlapped_parents = parents.iter().filter(|p| ctx.set.contains(**p)).count();

                // Remove the `high`. This segment cannot calculate
                // `children(high)`. If `high` matches a `parent` of
                // another segment, that segment will handle it.
                // This is related to the flat segment children path
                // below.
                let intersection = ctx
                    .set
                    .intersection(&span.into())
                    .difference(&span.high.into());

                if !seg.has_root()? {
                    // A segment must have at least one parent to be rootless.
                    debug_assert!(!parents.is_empty());
                    // Fast path: Take the entire segment directly.
                    if overlapped_parents == parents.len()
                        && intersection.count() + 1 == span.count()
                    {
                        trace(&|| format!(" push lv{} {:?} (rootless fast path)", level, &span));
                        ctx.result.push_span(span);
                        continue;
                    }
                }

                if !intersection.is_empty() {
                    if level > 0 {
                        visit_segments(ctx, span, level - 1)?;
                        continue;
                    } else {
                        // Flat segment children path.
                        // children(x..=y) = (x+1)..=(y+1) if x..=(y+1) is in a flat segment.
                        let seg_children = IdSet::from_spans(
                            intersection
                                .as_spans()
                                .iter()
                                .map(|s| s.low + 1..=s.high + 1),
                        );
                        trace(&|| format!(" push {:?} (flat segment range)", &seg_children));
                        ctx.result.push_set(&seg_children);
                    }
                }

                if overlapped_parents > 0 {
                    if level > 0 {
                        visit_segments(ctx, span, level - 1)?;
                    } else {
                        // child(any parent) = lowest id in this flat segment.
                        trace(&|| {
                            format!(" push {:?} (overlapped parents of flat segment)", &span.low)
                        });
                        ctx.result.push_span(span.low.into());
                    }
                }
            }
            Ok(())
        }

        let result_lower_bound = set.min().unwrap_or(Id::MAX);
        let mut ctx = Context {
            this: self,
            set,
            result_lower_bound,
            result: IdSet::empty(),
        };

        let max_level = self.max_level()?;
        for span in self.all()?.as_spans() {
            visit_segments(&mut ctx, *span, max_level)?;
        }

        trace(&|| format!(" result: {:?}", &ctx.result));

        Ok(ctx.result)
    }

    /// Calculate roots of the given set.
    fn roots(&self, set: IdSet) -> Result<IdSet> {
        Ok(set.difference(&self.children(set.clone())?))
    }

    /// Calculate one "greatest common ancestor" of the given set.
    ///
    /// If there are no common ancestors, return None.
    /// If there are multiple greatest common ancestors, pick one arbitrarily.
    /// Use `gca_all` to get all of them.
    fn gca_one(&self, set: IdSet) -> Result<Option<Id>> {
        // The set is sorted in DESC order. Therefore its first item can be used as the result.
        Ok(self.common_ancestors(set)?.max())
    }

    /// Calculate all "greatest common ancestor"s of the given set.
    /// `gca_one` is faster if an arbitrary answer is ok.
    fn gca_all(&self, set: IdSet) -> Result<IdSet> {
        self.heads_ancestors(self.common_ancestors(set)?)
    }

    /// Calculate all common ancestors of the given set.
    ///
    /// ```plain,ignore
    /// intersect(ancestors(i) for i in set)
    /// ```
    fn common_ancestors(&self, set: IdSet) -> Result<IdSet> {
        let result = match set.count() {
            0 => set,
            1 => self.ancestors(set)?,
            2 => {
                // Fast path that does not calculate "heads".
                let mut iter = set.iter();
                let a = iter.next().unwrap();
                let b = iter.next().unwrap();
                self.ancestors(a.into())?
                    .intersection(&self.ancestors(b.into())?)
            }
            _ => {
                // Try to reduce the size of `set`.
                // `common_ancestors(X)` = `common_ancestors(roots(X))`.
                let set = self.roots(set)?;
                set.iter()
                    .fold(Ok(IdSet::full()), |set: Result<IdSet>, id| {
                        Ok(set?.intersection(&self.ancestors(id.into())?))
                    })?
            }
        };
        Ok(result)
    }

    /// Test if `ancestor_id` is an ancestor of `descendant_id`.
    fn is_ancestor(&self, ancestor_id: Id, descendant_id: Id) -> Result<bool> {
        let set = self.ancestors(descendant_id.into())?;
        Ok(set.contains(ancestor_id))
    }

    /// Calculate "heads" of the ancestors of the given [`IdSet`]. That is,
    /// Find Y, which is the smallest subset of set X, where `ancestors(Y)` is
    /// `ancestors(X)`.
    ///
    /// This is faster than calculating `heads(ancestors(set))`.
    ///
    /// This is different from `heads`. In case set contains X and Y, and Y is
    /// an ancestor of X, but not the immediate ancestor, `heads` will include
    /// Y while this function won't.
    fn heads_ancestors(&self, set: IdSet) -> Result<IdSet> {
        let mut remaining = set;
        let mut result = IdSet::empty();
        while let Some(id) = remaining.max() {
            result.push_span((id..=id).into());
            // Remove ancestors reachable from that head.
            remaining = remaining.difference(&self.ancestors(id.into())?);
        }
        Ok(result)
    }

    /// Calculate the "dag range" - ids reachable from both sides.
    ///
    /// ```plain,ignore
    /// intersect(ancestors(heads), descendants(roots))
    /// ```
    ///
    /// This is O(flat segments), or O(merges).
    fn range(&self, roots: IdSet, mut heads: IdSet) -> Result<IdSet> {
        if roots.is_empty() {
            return Ok(IdSet::empty());
        }
        if heads.is_empty() {
            return Ok(IdSet::empty());
        }

        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::range", "{}", msg());
        }
        debug!(target: "dag::algo::range", "range({:?}, {:?})", &roots, &heads);

        // Remove uninteresting heads. Make `ancestors(heads)` a bit easier.
        let min_root_id = roots.min().unwrap();
        let min_head_id = heads.min().unwrap();
        if min_head_id < min_root_id {
            let span = min_root_id..=Id::MAX;
            heads = heads.intersection(&span.into());
            trace(&|| format!(" removed unreachable heads: {:?}", &heads));
        }

        let ancestors_of_heads = self.ancestors(heads)?;
        let result = self.descendants_intersection(&roots, &ancestors_of_heads)?;

        #[cfg(test)]
        {
            let intersection = ancestors_of_heads.intersection(&result);
            assert_eq!(result.as_spans(), intersection.as_spans());
        }

        trace(&|| format!(" result: {:?}", &result));
        Ok(result)
    }

    /// Calculate the descendants of the given set.
    ///
    /// Logically equivalent to `range(set, all())`.
    ///
    /// This is O(flat segments), or O(merges).
    fn descendants(&self, set: IdSet) -> Result<IdSet> {
        debug!(target: "dag::algo::descendants", "descendants({:?})", &set);
        let roots = set;
        let result = self.descendants_intersection(&roots, &self.all()?)?;
        trace!(target: "dag::algo::descendants", " result: {:?}", &result);
        Ok(result)
    }

    /// Calculate (descendants(roots) & ancestors).
    ///
    /// This is O(flat segments), or O(merges).
    ///
    /// `ancestors(ancestors)` must be equal to `ancestors`.
    fn descendants_intersection(&self, roots: &IdSet, ancestors: &IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::descendants_intersection", "{}", msg());
        }

        debug_assert_eq!(
            ancestors.count(),
            self.ancestors(ancestors.clone())?.count()
        );

        // Filter out roots that are not reachable from `ancestors`.
        let roots = ancestors.intersection(roots);
        let min_root = match roots.min() {
            Some(id) => id,
            None => return Ok(IdSet::empty()),
        };
        let max_root = roots.max().unwrap();

        // `result` could be initially `roots`. However that breaks ordering
        // (cannot use `result.push_span_asc` below).
        let mut result = IdSet::empty();

        // For the master group, use linear scan for flat segments. This is
        // usually more efficient, because the master group usually only has 1
        // head, and most segments will be included.
        let master_max_id = ancestors
            .max()
            .unwrap_or(Id::MIN)
            .min(Group::MASTER.max_id());
        for seg in self.iter_segments_ascending(min_root, 0)? {
            let seg = seg?;
            let span = seg.span()?;
            if span.low > master_max_id {
                break;
            }
            trace(&|| format!(" visit {:?}", &seg));
            let parents = seg.parents()?;
            let low = if !parents.is_empty()
                && parents
                    .iter()
                    .any(|&p| result.contains(p) || roots.contains(p))
            {
                // Include `span.low` if any (span.low's) parent is in the result set.
                span.low
            } else {
                // No parents in `result` set.
                match result
                    .intersection_span_min(span)
                    .or_else(|| roots.intersection(&span.into()).min())
                {
                    // `span` intersect partially with `roots | result`.
                    Some(id) => id,
                    None => continue,
                }
            };
            if low > master_max_id {
                break;
            }
            let result_span = IdSpan::from(low..=span.high);
            trace(&|| format!("  push {:?}", &result_span));
            result.push_span_asc(result_span);
        }

        // For the non-master group, only check flat segments covered by
        // `ancestors`.
        //
        // This is usually more efficient, because the non-master group can
        // have lots of heads (created in the past) that are no longer visible
        // or interesting. For a typical query like `x::y`, it might just select
        // a few heads in the non-master group. It's a waste of time to iterate
        // through lots of invisible segments.
        let non_master_spans = ancestors.intersection(
            &IdSpan::from(Group::NON_MASTER.min_id()..=Group::NON_MASTER.max_id()).into(),
        );
        // Visit in ascending order.
        let mut span_iter = non_master_spans.as_spans().iter().rev().cloned();
        let mut next_optional_span = span_iter.next();
        while let Some(next_span) = next_optional_span {
            // The "next_span" could be larger than a flat segment.
            let seg = match self.find_flat_segment_including_id(next_span.low)? {
                Some(seg) => seg,
                None => break,
            };
            let seg_span = seg.span()?;
            trace(&|| format!(" visit {:?} => {:?}", &next_span, &seg));
            // The overlap part of the flat segment and the span from 'ancestors'.
            let mut overlap_span =
                IdSpan::from(seg_span.low.max(next_span.low)..=seg_span.high.min(next_span.high));
            if roots.contains(overlap_span.low) {
                // Descendants includes 'overlap_span' since 'low' is in 'roots'.
                // (no need to check 'result' - it does not include anything in 'overlap')
                trace(&|| format!("  push {:?} (root contains low)", &overlap_span));
                result.push_span_asc(overlap_span);
            } else if next_span.low == seg_span.low {
                let parents = seg.parents()?;
                if !parents.is_empty()
                    && parents
                        .into_iter()
                        .any(|p| result.contains(p) || roots.contains(p))
                {
                    // Descendants includes 'overlap_span' since parents are in roots or result.
                    trace(&|| format!("  push {:?} (root contains parent)", &overlap_span));
                    result.push_span_asc(overlap_span);
                } else if overlap_span.low <= max_root && overlap_span.high >= min_root {
                    // If 'overlap_span' overlaps with roots, part of it should be in
                    // 'Descendants' result:
                    //
                    //            root1  root2
                    //               v    v
                    //    (low) |-- overlap-span --| (high)
                    //               |-------------|
                    //               push this part to result
                    let roots_intesection = roots.intersection(&overlap_span.into());
                    if let Some(id) = roots_intesection.min() {
                        overlap_span.low = id;
                        trace(&|| format!("  push {:?} (root in span)", &overlap_span));
                        result.push_span_asc(overlap_span);
                    }
                }
            } else {
                // This block practically does not happen if `ancestors` is
                // really "ancestors" (aka. `ancestors(ancestors)` is
                // `ancestors`), because `ancestors` will not include
                // a flat segment without including the segment's low id.
                //
                // But, in case it happens (because `ancestors` is weird),
                // do something sensible.

                // `next_span.low - 1` is the parent of `next_span.low`,
                debug_assert!(
                    false,
                    "ancestors in descendants_intersection is
                              not real ancestors"
                );
                let p = next_span.low - 1;
                if result.contains(p) || roots.contains(p) {
                    trace(&|| format!("  push {:?} ({:?} was included)", &overlap_span, p));
                    result.push_span_asc(overlap_span);
                }
            }
            // Update next_optional_span.
            next_optional_span = IdSpan::try_from_bounds(overlap_span.high + 1..=next_span.high)
                .or_else(|| span_iter.next());
        }

        trace(&|| format!(" intersect with {:?}", &ancestors));
        result = result.intersection(ancestors);

        Ok(result)
    }
}

impl<S: IdDagStore> IdDagAlgorithm for S {}

impl<Store: IdDagStore> Deref for IdDag<Store> {
    type Target = dyn IdDagAlgorithm;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

// Full IdMap -> Sparse IdMap
impl<Store: IdDagStore> IdDag<Store> {
    /// Copy a subset of "Universal" mapping from `full_idmap` to
    /// `sparse_idmap`. See [`IdDag::universal`].
    pub async fn write_sparse_idmap<M: crate::idmap::IdMapWrite>(
        &self,
        full_idmap: &dyn crate::ops::IdConvert,
        sparse_idmap: &mut M,
    ) -> Result<()> {
        for id in self.universal_ids()? {
            let name = full_idmap.vertex_name(id).await?;
            sparse_idmap.insert(id, name.as_ref()).await?
        }
        Ok(())
    }

    /// Return a subset of [`Id`]s that should be "Universal", including:
    ///
    /// - Heads of the master group.
    /// - Parents of merges (a merge is an id with multiple parents)
    ///   in the MASTER group.
    ///
    /// See also [`FirstAncestorConstraint::KnownUniversally`].
    ///
    /// Complexity: `O(flat segments)` for both time and space.
    pub fn universal_ids(&self) -> Result<BTreeSet<Id>> {
        let mut result = BTreeSet::new();
        for seg in self.next_segments(Id::MIN, 0)? {
            let parents = seg.parents()?;
            // Is it a merge?
            if parents.len() >= 2 {
                for id in parents {
                    debug_assert_eq!(id.group(), Group::MASTER);
                    result.insert(id);
                }
            }
        }
        for head in self.heads_ancestors(self.master_group()?)? {
            debug_assert_eq!(head.group(), Group::MASTER);
            result.insert(head);
        }
        Ok(result)
    }
}

/// There are many `x~n`s that all resolves to a single commit.
/// Constraint about `x~n`.
#[derive(Clone)]
pub enum FirstAncestorConstraint {
    /// No constraints.
    None,

    /// `x` and its slice is expected to be known both locally and remotely.
    ///
    /// Practically, this means `x` is either:
    /// - referred explicitly by `heads`.
    /// - a parent of a merge that is an ancestor of `heads`.
    ///   (a merge is a vertex with more than one parents)
    ///   (at clone and pull time, client gets a sparse idmap including them)
    ///
    /// This also enforces `x` to be part of `ancestors(heads)`.
    KnownUniversally { heads: IdSet },
}

impl<Store: IdDagStore> IdDag<Store> {
    /// Export non-master DAG as parent_id_func on HashMap.
    ///
    /// This can be expensive if there are a lot of non-master ids.
    /// It is currently only used to rebuild non-master groups after
    /// id re-assignment.
    pub fn non_master_parent_ids(&self) -> Result<HashMap<Id, Vec<Id>>> {
        let mut parents = HashMap::new();
        let start = Group::NON_MASTER.min_id();
        for seg in self.next_segments(start, 0)? {
            let span = seg.span()?;
            parents.insert(span.low, seg.parents()?);
            for i in (span.low + 1).to(span.high) {
                parents.insert(i, vec![i - 1]);
            }
        }
        Ok(parents)
    }

    /// Remove all non master Group identifiers from the DAG.
    pub fn remove_non_master(&mut self) -> Result<()> {
        // Non-append-only change. Use a new incompatible version.
        self.version = VerLink::new();
        self.store.remove_non_master()
    }
}

impl<Store: Persist> Persist for IdDag<Store> {
    type Lock = <Store as Persist>::Lock;

    fn lock(&mut self) -> Result<Self::Lock> {
        self.store.lock()
    }

    fn reload(&mut self, lock: &Self::Lock) -> Result<()> {
        self.store.reload(lock)
    }

    fn persist(&mut self, lock: &Self::Lock) -> Result<()> {
        self.store.persist(lock)
    }
}

impl<Store: IdDagStore> Debug for IdDag<Store> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let mut first = true;
        for level in 0..=self.max_level().unwrap_or_default() {
            if !first {
                write!(f, "\n")?;
            }
            first = false;
            write!(f, "Lv{}:", level)?;

            for group in Group::ALL.iter() {
                let segments = self.next_segments(group.min_id(), level).unwrap();
                if !segments.is_empty() {
                    for segment in segments {
                        write!(f, " {:?}", segment)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Lazily answer `any(...)`, `all(...)`.
struct LazyPredicate<P> {
    ids: Vec<Id>,
    predicate: P,
    true_count: usize,
    false_count: usize,
}

impl<P: Fn(Id) -> Result<bool>> LazyPredicate<P> {
    pub fn new(ids: Vec<Id>, predicate: P) -> Self {
        Self {
            ids,
            predicate,
            true_count: 0,
            false_count: 0,
        }
    }

    pub fn any(&mut self) -> Result<bool> {
        loop {
            if self.true_count > 0 {
                return Ok(true);
            }
            if self.true_count + self.false_count == self.ids.len() {
                return Ok(false);
            }
            self.test_one()?;
        }
    }

    pub fn all(&mut self) -> Result<bool> {
        loop {
            if self.true_count == self.ids.len() {
                return Ok(true);
            }
            if self.false_count > 0 {
                return Ok(false);
            }
            self.test_one()?;
        }
    }

    fn test_one(&mut self) -> Result<()> {
        let i = self.true_count + self.false_count;
        if (self.predicate)(self.ids[i])? {
            self.true_count += 1;
        } else {
            self.false_count += 1;
        }
        Ok(())
    }
}

fn default_seg_size() -> usize {
    DEFAULT_SEG_SIZE
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_segment_basic_lookups() {
        let dir = tempdir().unwrap();
        let mut dag = IdDag::open(dir.path()).unwrap();
        assert_eq!(dag.next_free_id(0, Group::MASTER).unwrap().0, 0);
        assert_eq!(dag.next_free_id(1, Group::MASTER).unwrap().0, 0);

        let flags = SegmentFlags::empty();

        dag.insert(flags, 0, Id::MIN, Id(50), &vec![]).unwrap();
        assert_eq!(dag.next_free_id(0, Group::MASTER).unwrap().0, 51);
        dag.insert(flags, 0, Id(51), Id(100), &vec![Id(50)])
            .unwrap();
        assert_eq!(dag.next_free_id(0, Group::MASTER).unwrap().0, 101);
        dag.insert(flags, 0, Id(101), Id(150), &vec![]).unwrap();
        assert_eq!(dag.next_free_id(0, Group::MASTER).unwrap().0, 151);
        assert_eq!(dag.next_free_id(1, Group::MASTER).unwrap().0, 0);
        dag.insert(flags, 1, Id::MIN, Id(150), &vec![]).unwrap();
        assert_eq!(dag.next_free_id(1, Group::MASTER).unwrap().0, 151);

        // Helper functions to make the below lines shorter.
        let low_by_head = |head, level| match dag.find_segment_by_head_and_level(Id(head), level) {
            Ok(Some(seg)) => seg.span().unwrap().low.0 as i64,
            Ok(None) => -1,
            _ => panic!("unexpected error"),
        };

        let low_by_id = |id| match dag.find_flat_segment_including_id(Id(id)) {
            Ok(Some(seg)) => seg.span().unwrap().low.0 as i64,
            Ok(None) => -1,
            _ => panic!("unexpected error"),
        };

        assert_eq!(low_by_head(0, 0), -1);
        assert_eq!(low_by_head(49, 0), -1);
        assert_eq!(low_by_head(50, 0), -1); // 0..=50 is merged into 0..=100.
        assert_eq!(low_by_head(51, 0), -1);
        assert_eq!(low_by_head(150, 0), 101);
        assert_eq!(low_by_head(100, 1), -1);

        assert_eq!(low_by_id(0), 0);
        assert_eq!(low_by_id(30), 0);
        assert_eq!(low_by_id(49), 0);
        assert_eq!(low_by_id(50), 0);
        assert_eq!(low_by_id(51), 0);
        assert_eq!(low_by_id(52), 0);
        assert_eq!(low_by_id(99), 0);
        assert_eq!(low_by_id(100), 0);
        assert_eq!(low_by_id(101), 101);
        assert_eq!(low_by_id(102), 101);
        assert_eq!(low_by_id(149), 101);
        assert_eq!(low_by_id(150), 101);
        assert_eq!(low_by_id(151), -1);
    }

    fn get_parents(id: Id) -> Result<Vec<Id>> {
        match id.0 {
            0 | 1 | 2 => Ok(Vec::new()),
            _ => Ok(vec![id - 1, Id(id.0 / 2)]),
        }
    }

    #[test]
    fn test_sync_reload() {
        let dir = tempdir().unwrap();
        let mut dag = IdDag::open(dir.path()).unwrap();
        assert_eq!(dag.next_free_id(0, Group::MASTER).unwrap().0, 0);

        let lock = dag.lock().unwrap();
        dag.reload(&lock).unwrap();
        dag.build_segments_volatile(Id(1001), &get_parents).unwrap();

        dag.persist(&lock).unwrap();
        drop(lock);

        assert_eq!(dag.max_level().unwrap(), 3);
        assert_eq!(
            dag.children(Id(1000).into())
                .unwrap()
                .iter()
                .collect::<Vec<Id>>(),
            vec![Id(1001)]
        );
    }

    #[test]
    fn test_all() {
        let dir = tempdir().unwrap();
        let mut dag = IdDag::open(dir.path()).unwrap();
        assert!(dag.all().unwrap().is_empty());
        dag.build_segments_volatile(Id(1001), &get_parents).unwrap();
        assert_eq!(dag.all().unwrap().count(), 1002);
    }

    #[test]
    fn test_flat_segments() {
        let dir = tempdir().unwrap();
        let test_dir = tempdir().unwrap();
        let mut dag = IdDag::open(dir.path()).unwrap();
        let mut test_dag = IdDag::open(test_dir.path()).unwrap();

        let empty_dag_segments = dag.flat_segments(Group::MASTER).unwrap();
        test_dag
            .build_segments_volatile_from_prepared_flat_segments(&empty_dag_segments)
            .unwrap();
        assert!(test_dag.all().unwrap().is_empty());

        dag.build_segments_volatile(Id(1001), &get_parents).unwrap();
        let flat_segments = dag.flat_segments(Group::MASTER).unwrap();
        test_dag
            .build_segments_volatile_from_prepared_flat_segments(&flat_segments)
            .unwrap();

        assert_eq!(test_dag.max_level().unwrap(), 3);
        assert_eq!(test_dag.all().unwrap().count(), 1002);

        let subset_flat_segments = dag
            .idset_to_flat_segments(IdSet::from_spans(vec![2..=4]))
            .unwrap();
        assert_eq!(subset_flat_segments.segments.len(), 3);
    }
}
