/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! # namedag
//!
//! Combination of IdMap and IdDag.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env::var;
use std::fmt;
use std::io;
use std::ops::Deref;
use std::sync::Arc;

use dag_types::FlatSegment;
use futures::future::join_all;
use futures::future::BoxFuture;
use futures::StreamExt;
use futures::TryStreamExt;
use nonblocking::non_blocking_result;
use parking_lot::Mutex;
use parking_lot::RwLock;

use crate::clone::CloneData;
use crate::errors::programming;
use crate::errors::DagError;
use crate::errors::NotFoundError;
use crate::id::Group;
use crate::id::Id;
use crate::id::VertexName;
use crate::iddag::IdDag;
use crate::iddag::IdDagAlgorithm;
use crate::iddagstore::IdDagStore;
use crate::idmap::CoreMemIdMap;
use crate::idmap::IdMapAssignHead;
use crate::idmap::IdMapWrite;
use crate::nameset::hints::Flags;
use crate::nameset::hints::Hints;
use crate::nameset::NameSet;
use crate::ops::CheckIntegrity;
use crate::ops::DagAddHeads;
use crate::ops::DagAlgorithm;
use crate::ops::DagExportCloneData;
use crate::ops::DagImportCloneData;
use crate::ops::DagImportPullData;
use crate::ops::DagPersistent;
use crate::ops::DagPullFastForwardMasterData;
use crate::ops::IdConvert;
use crate::ops::IdMapSnapshot;
use crate::ops::IntVersion;
use crate::ops::Open;
use crate::ops::Parents;
use crate::ops::Persist;
use crate::ops::PrefixLookup;
use crate::ops::ToIdSet;
use crate::ops::TryClone;
use crate::protocol;
use crate::protocol::is_remote_protocol_disabled;
use crate::protocol::AncestorPath;
use crate::protocol::Process;
use crate::protocol::RemoteIdConvertProtocol;
use crate::segment::PreparedFlatSegments;
use crate::segment::SegmentFlags;
use crate::IdSet;
use crate::Level;
use crate::Result;
use crate::VerLink;

#[cfg(any(test, feature = "indexedlog-backend"))]
mod indexedlog_namedag;
mod mem_namedag;

#[cfg(any(test, feature = "indexedlog-backend"))]
pub use indexedlog_namedag::IndexedLogNameDagPath;
#[cfg(any(test, feature = "indexedlog-backend"))]
pub use indexedlog_namedag::NameDag;
pub use mem_namedag::MemNameDag;
pub use mem_namedag::MemNameDagPath;

pub struct AbstractNameDag<I, M, P, S>
where
    I: Send + Sync,
    M: Send + Sync,
    P: Send + Sync,
    S: Send + Sync,
{
    pub(crate) dag: I,
    pub(crate) map: M,

    /// A read-only snapshot of the `NameDag`.
    /// Lazily calculated.
    snapshot: RwLock<Option<Arc<Self>>>,

    /// Heads added via `add_heads` that are not flushed yet.
    pending_heads: Vec<VertexName>,

    /// Path used to open this `NameDag`.
    path: P,

    /// Extra state of the `NameDag`.
    state: S,

    /// Identity of the dag. Derived from `path`.
    id: String,

    /// `Id`s that are persisted on disk. Used to answer `dirty()`.
    persisted_id_set: IdSet,

    /// Overlay IdMap. Used to store IdMap results resolved using remote
    /// protocols.
    overlay_map: Arc<RwLock<CoreMemIdMap>>,

    /// Max ID + 1 in the `overlay_map`. A protection. The `overlay_map` is
    /// shared (Arc) and its ID should not exceed the existing maximum ID at
    /// `map` open time. The IDs from 0..overlay_map_next_id are considered
    /// immutable, but lazy.
    overlay_map_next_id: Id,

    /// The source of `overlay_map`s. This avoids absolute Ids, and is
    /// used to flush overlay_map content shall the IdMap change on
    /// disk.
    overlay_map_paths: Arc<Mutex<Vec<(AncestorPath, Vec<VertexName>)>>>,

    /// Defines how to communicate with a remote service.
    /// The actual logic probably involves networking like HTTP etc
    /// and is intended to be implemented outside the `dag` crate.
    remote_protocol: Arc<dyn RemoteIdConvertProtocol>,

    /// A negative cache. Vertexes that are looked up remotely, and the remote
    /// confirmed the vertexes are outside the master group.
    missing_vertexes_confirmed_by_remote: Arc<RwLock<HashSet<VertexName>>>,
}

#[async_trait::async_trait]
impl<IS, M, P, S> DagPersistent for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore + Persist,
    IdDag<IS>: TryClone + 'static,
    M: TryClone + IdMapAssignHead + Persist + Send + Sync + 'static,
    P: Open<OpenTarget = Self> + Send + Sync + 'static,
    S: TryClone + IntVersion + Persist + Send + Sync + 'static,
{
    /// Add vertexes and their ancestors to the on-disk DAG.
    ///
    /// This is similar to calling `add_heads` followed by `flush`.
    /// But is faster.
    async fn add_heads_and_flush(
        &mut self,
        parent_names_func: &dyn Parents,
        master_names: &[VertexName],
        non_master_names: &[VertexName],
    ) -> Result<()> {
        if !self.pending_heads.is_empty() {
            return programming(format!(
                "ProgrammingError: add_heads_and_flush called with pending heads ({:?})",
                &self.pending_heads,
            ));
        }

        // Take lock.
        //
        // Reload meta and logs. This drops in-memory changes, which is fine because we have
        // checked there are no in-memory changes at the beginning.
        //
        // Also see comments in `NameDagState::lock()`.
        let old_version = self.state.int_version();
        let lock = self.state.lock()?;
        let map_lock = self.map.lock()?;
        let dag_lock = self.dag.lock()?;
        self.state.reload(&lock)?;
        let new_version = self.state.int_version();
        if old_version != new_version {
            self.invalidate_snapshot();
            self.invalidate_missing_vertex_cache();
            self.invalidate_overlay_map()?;
        }

        self.map.reload(&map_lock)?;
        self.dag.reload(&dag_lock)?;

        // Populate vertex negative cache to reduce round-trips doing remote lookups.
        // Release `self` from being mut borrowed while keeping the lock.
        if self.is_vertex_lazy() {
            let heads: Vec<VertexName> = master_names
                .iter()
                .cloned()
                .chain(non_master_names.iter().cloned())
                .collect();
            self.populate_missing_vertexes_for_add_heads(parent_names_func, &heads)
                .await?;
        }

        // Build.
        self.build(parent_names_func, master_names, non_master_names)
            .await?;

        // Write to disk.
        self.map.persist(&map_lock)?;
        self.dag.persist(&dag_lock)?;
        self.state.persist(&lock)?;
        drop(dag_lock);
        drop(map_lock);
        drop(lock);

        self.persisted_id_set = self.dag.all_ids_in_groups(&Group::ALL)?;
        debug_assert_eq!(self.dirty().await?.count().await?, 0);
        Ok(())
    }

    /// Write in-memory DAG to disk. This will also pick up changes to
    /// the DAG by other processes.
    ///
    /// This function re-assigns ids for vertexes. That requires the
    /// pending ids and vertexes to be non-lazy. If you're changing
    /// internal structures (ex. dag and map) directly, or introducing
    /// lazy vertexes, then avoid this function. Instead, lock and
    /// flush directly (see `add_heads_and_flush`, `import_clone_data`).
    async fn flush(&mut self, master_heads: &[VertexName]) -> Result<()> {
        // Sanity check.
        for result in self.vertex_id_batch(&master_heads).await? {
            result?;
        }

        // Write cached IdMap to disk.
        self.flush_cached_idmap().await?;

        // Constructs a new graph so we can copy pending data from the existing graph.
        let mut new_name_dag: Self = self.path.open()?;

        let parents: &(dyn DagAlgorithm + Send + Sync) = self;
        let non_master_heads = &self.pending_heads;
        let seg_size = self.dag.get_new_segment_size();
        new_name_dag.dag.set_new_segment_size(seg_size);
        new_name_dag.set_remote_protocol(self.remote_protocol.clone());
        new_name_dag.maybe_reuse_caches_from(self);
        new_name_dag
            .add_heads_and_flush(&parents, master_heads, non_master_heads)
            .await?;
        *self = new_name_dag;
        Ok(())
    }

    /// Write in-memory IdMap paths to disk so the next time we don't need to
    /// ask remote service for IdMap translation.
    #[tracing::instrument(skip(self))]
    async fn flush_cached_idmap(&self) -> Result<()> {
        // The map might have changed on disk. We cannot use the ids in overlay_map
        // directly. Instead, re-translate the paths.

        // Prepare data to insert. Do not hold Mutex across async yield points.
        let mut to_insert: Vec<(AncestorPath, Vec<VertexName>)> = Vec::new();
        std::mem::swap(&mut to_insert, &mut *self.overlay_map_paths.lock());
        if to_insert.is_empty() {
            return Ok(());
        }

        // Lock, reload from disk. Use a new state so the existing dag is not affected.
        tracing::debug!(target: "dag::cache", "flushing cached idmap ({} items)", to_insert.len());
        let mut new: Self = self.path.open()?;
        let lock = new.state.lock()?;
        let map_lock = new.map.lock()?;
        let dag_lock = new.dag.lock()?;
        new.state.reload(&lock)?;
        new.map.reload(&map_lock)?;
        new.dag.reload(&dag_lock)?;
        new.maybe_reuse_caches_from(self);

        let id_names =
            calculate_id_name_from_paths(&new.map, &*new.dag, new.overlay_map_next_id, &to_insert)
                .await?;

        // For testing purpose, skip inserting certain vertexes.
        let mut skip_vertexes: Option<HashSet<VertexName>> = None;
        if crate::is_testing() {
            if let Ok(s) = var("DAG_SKIP_FLUSH_VERTEXES") {
                skip_vertexes = Some(
                    s.split(",")
                        .filter_map(|s| VertexName::from_hex(s.as_bytes()).ok())
                        .collect(),
                )
            }
        }

        for (id, name) in id_names {
            if let Some(skip) = &skip_vertexes {
                if skip.contains(&name) {
                    tracing::info!(
                        target: "dag::cache",
                        "skip flushing {:?}-{} to IdMap set by DAG_SKIP_FLUSH_VERTEXES",
                        &name,
                        id
                    );
                    continue;
                }
            }
            tracing::debug!(target: "dag::cache", "insert {:?}-{} to IdMap", &name, id);
            new.map.insert(id, name.as_ref()).await?;
        }

        new.map.persist(&map_lock)?;
        new.state.persist(&lock)?;

        Ok(())
    }
}

impl<IS, M, P, S> AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: Send + Sync + 'static,
    M: Send + Sync + 'static,
    P: Send + Sync + 'static,
    S: IntVersion + Send + Sync + 'static,
{
    /// Attempt to reuse caches from `other` if two `NameDag`s are compatible.
    /// Usually called when `self` is newly created.
    fn maybe_reuse_caches_from(&mut self, other: &Self) {
        if self.state.int_version() != other.state.int_version()
            || self.overlay_map_next_id != other.overlay_map_next_id
        {
            tracing::debug!(target: "dag::cache", "cannot reuse cache");
            return;
        }
        tracing::debug!(
            target: "dag::cache", "reusing cache ({} missing)",
            other.missing_vertexes_confirmed_by_remote.read().len(),
        );
        self.missing_vertexes_confirmed_by_remote =
            other.missing_vertexes_confirmed_by_remote.clone();
        self.overlay_map = other.overlay_map.clone();
        self.overlay_map_paths = other.overlay_map_paths.clone();
    }
}

#[async_trait::async_trait]
impl<IS, M, P, S> DagAddHeads for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: TryClone + IdMapAssignHead + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    /// Add vertexes and their ancestors to the in-memory DAG.
    ///
    /// This does not write to disk. Use `add_heads_and_flush` to add heads
    /// and write to disk more efficiently.
    ///
    /// The added vertexes are immediately query-able. They will get Ids
    /// assigned to the NON_MASTER group internally. The `flush` function
    /// can re-assign Ids to the MASTER group.
    async fn add_heads(&mut self, parents: &dyn Parents, heads: &[VertexName]) -> Result<()> {
        self.invalidate_snapshot();

        // Populate vertex negative cache to reduce round-trips doing remote lookups.
        self.populate_missing_vertexes_for_add_heads(parents, heads)
            .await?;

        // Assign to the NON_MASTER group unconditionally so we can avoid the
        // complexity re-assigning non-master ids.
        //
        // This simplifies the API (not taking 2 groups), but comes with a
        // performance penalty - if the user does want to make one of the head
        // in the "master" group, we have to re-assign ids in flush().
        //
        // Practically, the callsite might want to use add_heads + flush
        // intead of add_heads_and_flush, if:
        // - The callsites cannot figure out "master_heads" at the same time
        //   it does the graph change. For example, hg might know commits
        //   before bookmark movements.
        // - The callsite is trying some temporary graph changes, and does
        //   not want to pollute the on-disk DAG. For example, calculating
        //   a preview of a rebase.
        let group = Group::NON_MASTER;

        // Update IdMap. Keep track of what heads are added.
        let mut outcome = PreparedFlatSegments::default();
        let mut covered = self.dag().all_ids_in_groups(&Group::ALL)?;
        for head in heads.iter() {
            if !self.contains_vertex_name(head).await? {
                let prepared_segments = self
                    .assign_head(head.clone(), parents, group, &mut covered, &IdSet::empty())
                    .await?;
                outcome.merge(prepared_segments);
                self.pending_heads.push(head.clone());
            }
        }

        // Update segments in the NON_MASTER group.
        self.dag
            .build_segments_volatile_from_prepared_flat_segments(&outcome)?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl<IS, M, P, S> IdMapWrite for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: TryClone + IdMapAssignHead + Send + Sync,
    P: TryClone + Send + Sync,
    S: TryClone + Send + Sync,
{
    async fn insert(&mut self, id: Id, name: &[u8]) -> Result<()> {
        self.map.insert(id, name).await
    }

    async fn remove_non_master(&mut self) -> Result<()> {
        self.map.remove_non_master().await
    }

    async fn need_rebuild_non_master(&self) -> bool {
        self.map.need_rebuild_non_master().await
    }
}

#[async_trait::async_trait]
impl<IS, M, P, S> DagImportCloneData for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore + Persist + 'static,
    IdDag<IS>: TryClone,
    M: TryClone + IdMapAssignHead + Persist + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Persist + Send + Sync + 'static,
{
    async fn import_clone_data(&mut self, clone_data: CloneData<VertexName>) -> Result<()> {
        // Write directly to disk. Bypassing "flush()" that re-assigns Ids
        // using parent functions.
        let (lock, map_lock, dag_lock) = self.reload()?;

        if !self.dag.all()?.is_empty() {
            return programming("Cannot import clone data for non-empty graph");
        }
        for (id, name) in clone_data.idmap {
            tracing::debug!(target: "dag::clone", "insert IdMap: {:?}-{:?}", &name, id);
            self.map.insert(id, name.as_ref()).await?;
        }
        self.dag
            .build_segments_volatile_from_prepared_flat_segments(&clone_data.flat_segments)?;

        self.verify_missing().await?;

        self.persist(lock, map_lock, dag_lock)
    }
}

impl<IS, M, P, S> AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore + Persist + 'static,
    IdDag<IS>: TryClone,
    M: TryClone + IdMapAssignHead + Persist + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Persist + Send + Sync + 'static,
{
    /// Verify that universally known vertexes and heads are present in IdMap.
    async fn verify_missing(&self) -> Result<()> {
        let missing: Vec<Id> = self.check_universal_ids().await?;
        if !missing.is_empty() {
            let msg = format!(
                concat!(
                    "Clone data does not contain vertex for {:?}. ",
                    "This is most likely a server-side bug."
                ),
                missing,
            );
            return programming(msg);
        }

        Ok(())
    }

    fn reload(&mut self) -> Result<(S::Lock, M::Lock, IS::Lock)> {
        let lock = self.state.lock()?;
        let map_lock = self.map.lock()?;
        let dag_lock = self.dag.lock()?;
        self.state.reload(&lock)?;
        self.map.reload(&map_lock)?;
        self.dag.reload(&dag_lock)?;

        Ok((lock, map_lock, dag_lock))
    }

    fn persist(&mut self, lock: S::Lock, map_lock: M::Lock, dag_lock: IS::Lock) -> Result<()> {
        self.map.persist(&map_lock)?;
        self.dag.persist(&dag_lock)?;
        self.state.persist(&lock)?;

        self.invalidate_overlay_map()?;
        self.persisted_id_set = self.dag.all_ids_in_groups(&Group::ALL)?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl<IS, M, P, S> DagImportPullData for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore + Persist,
    IdDag<IS>: TryClone,
    M: TryClone + IdMapAssignHead + Persist + Send + Sync + 'static,
    P: Open<OpenTarget = Self> + TryClone + Send + Sync + 'static,
    S: IntVersion + TryClone + Persist + Send + Sync + 'static,
{
    async fn import_pull_data(&mut self, clone_data: CloneData<VertexName>) -> Result<()> {
        if !self.pending_heads.is_empty() {
            return programming(format!(
                "import_pull_data called with pending heads ({:?})",
                &self.pending_heads,
            ));
        }

        if let Some(highest_seg) = clone_data.flat_segments.segments.last() {
            let id = highest_seg.high;
            if !clone_data.idmap.contains_key(&id) {
                return programming(format!("server does not provide name for head {:?}", id));
            }
        }

        // Constructs a new graph so we don't expose a broken `self` state on error.
        let mut new: Self = self.path.open()?;
        let (lock, map_lock, dag_lock) = new.reload()?;
        new.set_remote_protocol(self.remote_protocol.clone());
        new.maybe_reuse_caches_from(self);

        // Parents that should exist in the local graph. Look them up in 1 round-trip
        // and insert to the local graph.
        // Also check that roots of the new segments do not overlap with the local graph.
        // For example,
        //
        //      D          When the client has B (and A, C), and is pulling D,
        //     /|\         the server provides D, E, F, with parents B and C,
        //    F B E        and roots F and E.
        //      |\|        The client must have B and C, and must not have F
        //      A C        or E.
        {
            let mut root_ids: Vec<Id> = Vec::new();
            let mut parent_ids: Vec<Id> = Vec::new();
            let segments = &clone_data.flat_segments.segments;
            let id_set = IdSet::from_spans(segments.iter().map(|s| s.low..=s.high));
            for seg in segments {
                let pids: Vec<Id> = seg.parents.iter().copied().collect();
                // Parents that are not part of the pull vertexes should exist
                // in the local graph.
                let connected_pids: Vec<Id> = pids
                    .iter()
                    .copied()
                    .filter(|&p| !id_set.contains(p))
                    .collect();
                if connected_pids.len() == pids.len() {
                    // The "low" of the segment is a root (of vertexes to insert).
                    // It needs an overlap check.
                    root_ids.push(seg.low);
                }
                parent_ids.extend(connected_pids);
            }

            let to_names = |ids: &[Id], hint: &str| -> Result<Vec<VertexName>> {
                let names = ids.iter().map(|i| match clone_data.idmap.get(&i) {
                    Some(v) => Ok(v.clone()),
                    None => {
                        programming(format!("server does not provide name for {} {:?}", hint, i))
                    }
                });
                names.collect()
            };

            let parent_names = to_names(&parent_ids, "parent")?;
            let root_names = to_names(&root_ids, "root")?;
            tracing::trace!(
                "pull: connected parents: {:?}, roots: {:?}",
                &parent_names,
                &root_names
            );

            // Pre-lookup in one round-trip.
            let mut names = parent_names
                .iter()
                .chain(root_names.iter())
                .cloned()
                .collect::<Vec<_>>();
            names.sort_unstable();
            names.dedup();
            let resolved = new.vertex_id_batch(&names).await?;
            assert_eq!(resolved.len(), names.len());
            for (id, name) in resolved.into_iter().zip(names) {
                if let Ok(id) = id {
                    if !new.map.contains_vertex_name(&name).await? {
                        tracing::debug!(target: "dag::pull", "insert IdMap: {:?}-{:?}", &name, id);
                        new.map.insert(id, name.as_ref()).await?;
                    }
                }
            }

            for name in root_names {
                if new.contains_vertex_name(&name).await? {
                    let e = crate::Error::NeedSlowPath(format!("{:?} exists in local graph", name));
                    return Err(e);
                }
            }

            let client_parents = new.vertex_id_batch(&parent_names).await?;
            client_parents.into_iter().collect::<Result<Vec<Id>>>()?;
        }

        let mut next_free_client_id = new.dag.next_free_id(0, Group::MASTER)?;
        let mut new_client_segments = vec![];
        let server_idmap_tree: BTreeMap<_, _> = clone_data.idmap.clone().into_iter().collect();
        let mut last_server_id = None; // Can't use 0 since server might return segment starting from 0 (for example if pulling from empty repo)

        for server_segment in clone_data.flat_segments.segments {
            if server_segment.low > server_segment.high {
                return programming(format!(
                    "server returned incorrect segment {:?}",
                    server_segment
                ));
            }
            match last_server_id {
                Some(last_server_id) if server_segment.low <= last_server_id => {
                    return programming(format!(
                        "server returned non sorted segment {:?}, previous segment high {}",
                        server_segment, last_server_id
                    ));
                }
                _ => {}
            }
            last_server_id = Some(server_segment.high);
            let mut parent_names = vec![];
            for server_parent in server_segment.parents {
                let parent_name = clone_data.idmap.get(&server_parent);
                // all parents should be in server's id_map
                let parent_name = parent_name.ok_or_else(|| {
                    DagError::Programming(format!(
                        "server does not provide name for id {}",
                        server_parent
                    ))
                })?;
                parent_names.push(parent_name.clone());
            }
            // Parents should exist in the local graph and can be resolved without looking
            // up remotely. Either looked up above, or inserted by the `new.map.insert`
            // loop below.
            let client_parents = new.map.vertex_id_batch(&parent_names).await?;
            let client_parents = client_parents.into_iter().collect::<Result<Vec<Id>>>()?;

            let new_client_id_low = next_free_client_id;
            let new_client_id_high =
                new_client_id_low + server_segment.high.0 - server_segment.low.0;
            next_free_client_id = new_client_id_high + 1;
            new_client_segments.push(FlatSegment {
                low: new_client_id_low,
                high: new_client_id_high,
                parents: client_parents,
            });

            // this can be negative becase we generally don't know if client id's are greater or lower then server id's
            let server_to_client_offset = new_client_id_low.0 as i64 - server_segment.low.0 as i64;

            let new_server_ids = server_idmap_tree.range(server_segment.low..=server_segment.high);

            for (server_id, name) in new_server_ids {
                let client_id = Id((server_id.0 as i64 + server_to_client_offset) as u64);
                tracing::debug!(target: "dag::pull", "insert IdMap: {:?}-{:?}", &name, client_id);
                new.map.insert(client_id, name.as_ref()).await?;
            }
        }

        let new_client_segments = PreparedFlatSegments {
            segments: new_client_segments,
        };

        new.dag
            .build_segments_volatile_from_prepared_flat_segments(&new_client_segments)?;

        if cfg!(debug_assertions) {
            new.verify_missing().await?;
        }

        new.persist(lock, map_lock, dag_lock)?;
        *self = new;
        Ok(())
    }
}

#[async_trait::async_trait]
impl<IS, M, P, S> DagExportCloneData for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: IdConvert + TryClone + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    async fn export_clone_data(&self) -> Result<CloneData<VertexName>> {
        let idmap: HashMap<Id, VertexName> = {
            let ids: Vec<Id> = self.dag.universal_ids()?.into_iter().collect();
            tracing::debug!("export: {} universally known vertexes", ids.len());
            let names = {
                let fallible_names = self.vertex_name_batch(&ids).await?;
                let mut names = Vec::with_capacity(fallible_names.len());
                for name in fallible_names {
                    names.push(name?);
                }
                names
            };
            ids.into_iter().zip(names).collect()
        };

        let flat_segments: PreparedFlatSegments = {
            let segments = self.dag.next_segments(Id::MIN, 0)?;
            let mut prepared = Vec::with_capacity(segments.len());
            for segment in segments {
                let span = segment.span()?;
                let parents = segment.parents()?;
                prepared.push(FlatSegment {
                    low: span.low,
                    high: span.high,
                    parents,
                });
            }
            PreparedFlatSegments { segments: prepared }
        };

        let data = CloneData {
            flat_segments,
            idmap,
        };
        Ok(data)
    }
}

#[async_trait::async_trait]
impl<IS, M, P, S> DagPullFastForwardMasterData for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: IdConvert + TryClone + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    async fn pull_fast_forward_master(
        &self,
        old_master: VertexName,
        new_master: VertexName,
    ) -> Result<CloneData<VertexName>> {
        let old = self.map.vertex_id(old_master).await?;
        let new = self.map.vertex_id(new_master).await?;
        let master_group = self.dag.master_group()?;

        if !master_group.contains(old) {
            return programming(format!("old vertex {} is not in master group", old));
        }

        if !master_group.contains(new) {
            return programming(format!("new vertex {} is not in master group", new));
        }

        let old_ancestors = self.dag.ancestors(old.into())?;
        let new_ancestors = self.dag.ancestors(new.into())?;

        let missing_set = new_ancestors.difference(&old_ancestors);
        let flat_segments = self.dag.idset_to_flat_segments(missing_set)?;
        let ids: Vec<_> = flat_segments.parents_head_and_roots().into_iter().collect();

        let idmap: HashMap<Id, VertexName> = {
            tracing::debug!("pull: {} vertexes in idmap", ids.len());
            let names = {
                let fallible_names = self.vertex_name_batch(&ids).await?;
                let mut names = Vec::with_capacity(fallible_names.len());
                for name in fallible_names {
                    names.push(name?);
                }
                names
            };
            assert_eq!(ids.len(), names.len());
            ids.into_iter().zip(names).collect()
        };

        let data = CloneData {
            flat_segments,
            idmap,
        };
        Ok(data)
    }
}

impl<IS, M, P, S> AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: TryClone + Send + Sync,
    P: TryClone + Send + Sync,
    S: TryClone + Send + Sync,
{
    /// Invalidate cached content. Call this before changing the graph
    /// so `version` in `snapshot` is dropped, and `version.bump()` might
    /// have a faster path.
    ///
    /// Forgetting to call this function might hurt performance a bit, but does
    /// not affect correctness.
    fn invalidate_snapshot(&mut self) {
        *self.snapshot.write() = None;
    }

    fn invalidate_missing_vertex_cache(&mut self) {
        tracing::debug!(target: "dag::cache", "cleared missing cache");
        *self.missing_vertexes_confirmed_by_remote.write() = Default::default();
    }

    fn invalidate_overlay_map(&mut self) -> Result<()> {
        self.overlay_map = Default::default();
        self.update_overlay_map_next_id()?;
        tracing::debug!(target: "dag::cache", "cleared overlay map cache");
        Ok(())
    }

    fn update_overlay_map_next_id(&mut self) -> Result<()> {
        let next_id = self.dag.next_free_id(0, Group::MASTER)?;
        self.overlay_map_next_id = next_id;
        Ok(())
    }

    /// Attempt to get a snapshot of this graph.
    pub(crate) fn try_snapshot(&self) -> Result<Arc<Self>> {
        if let Some(s) = self.snapshot.read().deref() {
            if s.dag.version() == self.dag.version() {
                return Ok(Arc::clone(s));
            }
        }

        let mut snapshot = self.snapshot.write();
        match snapshot.deref() {
            Some(s) if s.dag.version() == self.dag.version() => Ok(s.clone()),
            _ => {
                let cloned = Self {
                    dag: self.dag.try_clone()?,
                    map: self.map.try_clone()?,
                    snapshot: Default::default(),
                    pending_heads: self.pending_heads.clone(),
                    persisted_id_set: self.persisted_id_set.clone(),
                    path: self.path.try_clone()?,
                    state: self.state.try_clone()?,
                    id: self.id.clone(),
                    // If we do deep clone here we can remove `overlay_map_next_id`
                    // protection. However that could be too expensive.
                    overlay_map: Arc::clone(&self.overlay_map),
                    overlay_map_next_id: self.overlay_map_next_id,
                    overlay_map_paths: Arc::clone(&self.overlay_map_paths),
                    remote_protocol: self.remote_protocol.clone(),
                    missing_vertexes_confirmed_by_remote: Arc::clone(
                        &self.missing_vertexes_confirmed_by_remote,
                    ),
                };
                let result = Arc::new(cloned);
                *snapshot = Some(Arc::clone(&result));
                Ok(result)
            }
        }
    }

    pub fn dag(&self) -> &IdDag<IS> {
        &self.dag
    }

    pub fn map(&self) -> &M {
        &self.map
    }

    /// Set the remote protocol for converting between Id and Vertex remotely.
    ///
    /// This is usually used on "sparse" ("lazy") Dag where the IdMap is incomplete
    /// for vertexes in the master groups.
    pub fn set_remote_protocol(&mut self, protocol: Arc<dyn RemoteIdConvertProtocol>) {
        self.remote_protocol = protocol;
    }

    pub(crate) fn get_remote_protocol(&self) -> Arc<dyn RemoteIdConvertProtocol> {
        self.remote_protocol.clone()
    }
}

impl<IS, M, P, S> AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: TryClone + IdMapAssignHead + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    async fn populate_missing_vertexes_for_add_heads(
        &mut self,
        parents: &dyn Parents,
        heads: &[VertexName],
    ) -> Result<()> {
        if self.is_vertex_lazy() {
            let unassigned = calculate_definitely_unassigned_vertexes(self, parents, heads).await?;
            let mut missing = self.missing_vertexes_confirmed_by_remote.write();
            for v in unassigned {
                if missing.insert(v.clone()) {
                    tracing::trace!(target: "dag::cache", "cached missing {:?} (definitely missing)", &v);
                }
            }
        }
        Ok(())
    }
}

/// Calculate vertexes that are definitely not assigned (not in the IdMap,
/// and not in the lazy part of the IdMap) according to
/// `hint_pending_subdag`. This does not report all unassigned vertexes.
/// But the reported vertexes are guaranteed not assigned.
///
/// If X is assigned, then X's parents must have been assigned.
/// If X is not assigned, then all X's descendants are not assigned.
///
/// This function visits the "roots" of "parents", and if they are not assigned,
/// then add their descendants to the "unassigned" result set.
async fn calculate_definitely_unassigned_vertexes<IS, M, P, S>(
    this: &AbstractNameDag<IdDag<IS>, M, P, S>,
    parents: &dyn Parents,
    heads: &[VertexName],
) -> Result<Vec<VertexName>>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: TryClone + IdMapAssignHead + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    // subdag: vertexes to insert
    //
    // For example, when adding C---D to the graph A---B:
    //
    //      A---B
    //           \
    //            C---D
    //
    // The subdag is C---D (C does not have parent).
    //
    // Extra checks are needed because upon reload, the main graph
    // A---B might already contain part of the subdag to be added.
    let subdag = parents.hint_subdag_for_insertion(heads).await?;

    let mut remaining = subdag.all().await?;
    let mut unassigned = NameSet::empty();

    // For lazy graph, avoid some remote lookups by figuring out
    // some definitely unassigned (missing) vertexes. For example,
    //
    //      A---B---C
    //           \
    //            D---E
    //
    // When adding D---E (subdag, new vertex that might trigger remote
    // lookup) with parent B to the main graph (A--B--C),
    // 1. If B exists, and is not in the master group, then B and its
    //    descendants cannot be not lazy, and there is no need to lookup
    //    D remotely.
    // 2. If B exists, and is in the master group, and all its children
    //    except D (i.e. C) are known locally, and the vertex name of D
    //    does not match other children (C), we know that D cannot be
    //    in the lazy part of the main graph, and can skip the remote
    //    lookup.
    let mut unassigned_roots = Vec::new();
    if this.is_vertex_lazy() {
        let roots = subdag.roots(remaining.clone()).await?;
        let mut roots_iter = roots.iter().await?;
        while let Some(root) = roots_iter.next().await {
            let root = root?;

            // Do a local "contains" check.
            if matches!(
                &this.contains_vertex_name_locally(&[root.clone()]).await?[..],
                [true]
            ) {
                tracing::debug!(target: "dag::definitelymissing", "root {:?} is already known", &root);
                continue;
            }

            let root_parents_id_set = {
                let root_parents = parents.parent_names(root.clone()).await?;
                let root_parents_set = match this
                    .sort(&NameSet::from_static_names(root_parents))
                    .await
                {
                    Ok(set) => set,
                    Err(_) => {
                        tracing::trace!(target: "dag::definitelymissing", "root {:?} is unclear (parents cannot be resolved)", &root);
                        continue;
                    }
                };
                this.to_id_set(&root_parents_set).await?
            };

            // If there are no parents of `root`, we cannot confidently test
            // whether `root` is missing or not.
            if root_parents_id_set.is_empty() {
                tracing::trace!(target: "dag::definitelymissing", "root {:?} is unclear (no parents)", &root);
                continue;
            }

            // All parents of `root` are non-lazy.
            // So `root` is non-lazy and the local "contains" check is the same
            // as a remote "contains" check.
            if root_parents_id_set
                .iter()
                .all(|i| i.group() == Group::NON_MASTER)
            {
                tracing::debug!(target: "dag::definitelymissing", "root {:?} is not assigned (non-lazy parent)", &root);
                unassigned_roots.push(root);
                continue;
            }

            // All children of lazy parents of `root` are known locally.
            // So `root` cannot match an existing vertex in the lazy graph.
            let children_ids: Vec<Id> = this.dag.children(root_parents_id_set)?.iter().collect();
            if this
                .map
                .contains_vertex_id_locally(&children_ids)
                .await?
                .iter()
                .all(|b| *b)
            {
                tracing::debug!(target: "dag::definitelymissing", "root {:?} is not assigned (children of parents are known)", &root);
                unassigned_roots.push(root);
                continue;
            }

            tracing::trace!(target: "dag::definitelymissing", "root {:?} is unclear", &root);
        }

        if !unassigned_roots.is_empty() {
            unassigned = subdag
                .descendants(NameSet::from_static_names(unassigned_roots))
                .await?;
            remaining = remaining.difference(&unassigned);
        }
    }

    // Figure out unassigned (missing) vertexes that do need to be inserted.
    //
    // remaining:  vertexes to query.
    // unassigned: vertexes known unassigned.
    // assigned:   vertexes known assigned.
    //
    // This is similar to hg pull/push exchange. In short, loop until "remaining" becomes empty:
    // - Take a subset of "remaining".
    // - Check the subset. Divide it into (subset_assigned, subset_unassigned).
    // - Include ancestors(subset_assigned) in "assigned".
    // - Include descendants(subset_unassigned) in "unassigned".
    // - Exclude "assigned" and "unassigned" from "remaining".

    for i in 1usize.. {
        let remaining_old_len = remaining.count().await?;
        if remaining_old_len == 0 {
            break;
        }

        // Sample: heads, roots, and the "middle point" from "remaining".
        let sample = if i <= 2 {
            // But for the first few queries, let's just check the roots.
            // This could reduce remote lookups, when we only need to
            // query the roots to rule out all `remaining` vertexes.
            subdag.roots(remaining.clone()).await?
        } else {
            subdag
                .roots(remaining.clone())
                .await?
                .union(&subdag.heads(remaining.clone()).await?)
                .union(&remaining.skip((remaining_old_len as u64) / 2).take(1))
        };
        let sample: Vec<VertexName> = sample.iter().await?.try_collect().await?;
        let assigned_bools: Vec<bool> = {
            let ids = this.vertex_id_batch(&sample).await?;
            ids.into_iter().map(|i| i.is_ok()).collect()
        };
        debug_assert_eq!(sample.len(), assigned_bools.len());

        let mut new_assigned = Vec::with_capacity(sample.len());
        let mut new_unassigned = Vec::with_capacity(sample.len());
        for (v, b) in sample.into_iter().zip(assigned_bools) {
            if b {
                new_assigned.push(v);
            } else {
                new_unassigned.push(v);
            }
        }
        let new_assigned = NameSet::from_static_names(new_assigned);
        let new_unassigned = NameSet::from_static_names(new_unassigned);

        let new_assigned = subdag.ancestors(new_assigned).await?;
        let new_unassigned = subdag.descendants(new_unassigned).await?;

        remaining = remaining.difference(&new_assigned.union(&new_unassigned));
        let remaining_new_len = remaining.count().await?;

        let unassigned_old_len = unassigned.count().await?;
        unassigned = unassigned.union(&subdag.descendants(new_unassigned).await?);
        let unassigned_new_len = unassigned.count().await?;

        tracing::trace!(
            target: "dag::definitelymissing",
            "#{} remaining {} => {}, unassigned: {} => {}",
            i,
            remaining_old_len,
            remaining_new_len,
            unassigned_old_len,
            unassigned_new_len
        );
    }
    tracing::debug!(target: "dag::definitelymissing", "unassigned (missing): {:?}", &unassigned);

    let unassigned = unassigned.iter().await?.try_collect().await?;
    Ok(unassigned)
}

// The "client" Dag. Using a remote protocol to fill lazy part of the vertexes.
impl<IS, M, P, S> AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: IdConvert + TryClone + Send + Sync,
    P: TryClone + Send + Sync,
    S: TryClone + Send + Sync,
{
    /// Resolve vertexes remotely and cache the result in the overlay map.
    /// Return the resolved ids in the given order. Not all names are resolved.
    async fn resolve_vertexes_remotely(&self, names: &[VertexName]) -> Result<Vec<Option<Id>>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        if is_remote_protocol_disabled() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "resolving vertexes remotely disabled",
            )
            .into());
        }
        if names.len() < 30 {
            tracing::debug!(target: "dag::protocol", "resolve names {:?} remotely", &names);
        } else {
            tracing::debug!(target: "dag::protocol", "resolve names ({}) remotely", names.len());
        }
        crate::failpoint!("dag-resolve-vertexes-remotely");
        let request: protocol::RequestNameToLocation =
            (self.map(), self.dag()).process(names.to_vec()).await?;
        let path_names = self
            .remote_protocol
            .resolve_names_to_relative_paths(request.heads, request.names)
            .await?;
        self.insert_relative_paths(path_names).await?;
        let overlay = self.overlay_map.read();
        let mut ids = Vec::with_capacity(names.len());
        let mut missing = self.missing_vertexes_confirmed_by_remote.write();
        for name in names {
            if let Some(id) = overlay.lookup_vertex_id(name) {
                ids.push(Some(id));
            } else {
                tracing::trace!(target: "dag::cache", "cached missing {:?} (server confirmed)", &name);
                missing.insert(name.clone());
                ids.push(None);
            }
        }
        Ok(ids)
    }

    /// Resolve ids remotely and cache the result in the overlay map.
    /// Return the resolved ids in the given order. All ids must be resolved.
    async fn resolve_ids_remotely(&self, ids: &[Id]) -> Result<Vec<VertexName>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if is_remote_protocol_disabled() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "resolving ids remotely disabled",
            )
            .into());
        }
        if ids.len() < 30 {
            tracing::debug!(target: "dag::protocol", "resolve ids {:?} remotely", &ids);
        } else {
            tracing::debug!(target: "dag::protocol", "resolve ids ({}) remotely", ids.len());
        }
        crate::failpoint!("dag-resolve-ids-remotely");
        let request: protocol::RequestLocationToName = (self.map(), self.dag())
            .process(IdSet::from_spans(ids.iter().copied()))
            .await?;
        let path_names = self
            .remote_protocol
            .resolve_relative_paths_to_names(request.paths)
            .await?;
        self.insert_relative_paths(path_names).await?;
        let overlay = self.overlay_map.read();
        let mut names = Vec::with_capacity(ids.len());
        for &id in ids {
            if let Some(name) = overlay.lookup_vertex_name(id) {
                names.push(name);
            } else {
                return id.not_found();
            }
        }
        Ok(names)
    }

    /// Insert `x~n` relative paths to the overlay IdMap.
    async fn insert_relative_paths(
        &self,
        path_names: Vec<(AncestorPath, Vec<VertexName>)>,
    ) -> Result<()> {
        if path_names.is_empty() {
            return Ok(());
        }
        let to_insert: Vec<(Id, VertexName)> = calculate_id_name_from_paths(
            self.map(),
            self.dag().deref(),
            self.overlay_map_next_id,
            &path_names,
        )
        .await?;

        let mut paths = self.overlay_map_paths.lock();
        paths.extend(path_names);
        drop(paths);

        let mut overlay = self.overlay_map.write();
        for (id, name) in to_insert {
            tracing::trace!(target: "dag::cache", "cached mapping {:?} <=> {:?}", id, &name);
            overlay.insert_vertex_id_name(id, name);
        }

        Ok(())
    }
}

/// Calculate (id, name) pairs to insert from (path, [name]) pairs.
async fn calculate_id_name_from_paths(
    map: &dyn IdConvert,
    dag: &dyn IdDagAlgorithm,
    max_id_plus_1: Id,
    path_names: &[(AncestorPath, Vec<VertexName>)],
) -> Result<Vec<(Id, VertexName)>> {
    if path_names.is_empty() {
        return Ok(Vec::new());
    }
    let mut to_insert: Vec<(Id, VertexName)> =
        Vec::with_capacity(path_names.iter().map(|(_, ns)| ns.len()).sum());
    for (path, names) in path_names {
        if names.is_empty() {
            continue;
        }
        // Resolve x~n to id. x is "universally known" so it should exist locally.
        let x_id = map.vertex_id(path.x.clone()).await.map_err(|e| {
            let msg = format!(
                concat!(
                    "Cannot resolve x ({:?}) in x~n locally. The x is expected to be known ",
                    "locally and is populated at clone time. This x~n is used to convert ",
                    "{:?} to a location in the graph. (Check initial clone logic) ",
                    "(Error: {})",
                ),
                &path.x, &names[0], e
            );
            crate::Error::Programming(msg)
        })?;
        tracing::trace!(
            "resolve path {:?} names {:?} (x = {}) to overlay",
            &path,
            &names,
            x_id
        );
        if x_id >= max_id_plus_1 {
            crate::failpoint!("dag-error-x-n-overflow");
            let msg = format!(
                concat!(
                    "Server returned x~n (x = {:?} {}, n = {}). But x exceeds the head in the ",
                    "local master group {}. This is not expected and indicates some ",
                    "logic error on the server side."
                ),
                &path.x, x_id, path.n, max_id_plus_1
            );
            return programming(msg);
        }
        let mut id = match dag.first_ancestor_nth(x_id, path.n).map_err(|e| {
            let msg = format!(
                concat!(
                    "Cannot resolve x~n (x = {:?} {}, n = {}): {}. ",
                    "This indicates the client-side graph is somewhat incompatible from the ",
                    "server-side graph. Something (server-side or client-side) was probably ",
                    "seriously wrong before this error."
                ),
                &path.x, x_id, path.n, e
            );
            crate::Error::Programming(msg)
        }) {
            Err(e) => {
                crate::failpoint!("dag-error-x-n-unresolvable");
                return Err(e);
            }
            Ok(id) => id,
        };
        if names.len() < 30 {
            tracing::debug!("resolved {:?} => {} {:?}", &path, id, &names);
        } else {
            tracing::debug!("resolved {:?} => {} {:?} ...", &path, id, &names[0]);
        }
        for (i, name) in names.into_iter().enumerate() {
            if i > 0 {
                // Follow id's first parent.
                id = match dag.parent_ids(id)?.first().cloned() {
                    Some(id) => id,
                    None => {
                        let msg = format!(
                            concat!(
                                "Cannot resolve x~(n+i) (x = {:?} {}, n = {}, i = {}) locally. ",
                                "This indicates the client-side graph is somewhat incompatible ",
                                "from the server-side graph. Something (server-side or ",
                                "client-side) was probably seriously wrong before this error."
                            ),
                            &path.x, x_id, path.n, i
                        );
                        return programming(msg);
                    }
                }
            }

            tracing::trace!(" resolved {:?} = {:?}", id, &name,);
            to_insert.push((id, name.clone()));
        }
    }
    Ok(to_insert)
}

// The server Dag. IdMap is complete. Provide APIs for client Dag to resolve vertexes.
// Currently mainly used for testing purpose.
#[async_trait::async_trait]
impl<IS, M, P, S> RemoteIdConvertProtocol for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: IdConvert + TryClone + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    async fn resolve_names_to_relative_paths(
        &self,
        heads: Vec<VertexName>,
        names: Vec<VertexName>,
    ) -> Result<Vec<(AncestorPath, Vec<VertexName>)>> {
        let request = protocol::RequestNameToLocation { names, heads };
        let response: protocol::ResponseIdNamePair =
            (self.map(), self.dag()).process(request).await?;
        Ok(response.path_names)
    }

    async fn resolve_relative_paths_to_names(
        &self,
        paths: Vec<AncestorPath>,
    ) -> Result<Vec<(AncestorPath, Vec<VertexName>)>> {
        let request = protocol::RequestLocationToName { paths };
        let response: protocol::ResponseIdNamePair =
            (self.map(), self.dag()).process(request).await?;
        Ok(response.path_names)
    }
}

// On "snapshot".
#[async_trait::async_trait]
impl<IS, M, P, S> RemoteIdConvertProtocol for Arc<AbstractNameDag<IdDag<IS>, M, P, S>>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: IdConvert + TryClone + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    async fn resolve_names_to_relative_paths(
        &self,
        heads: Vec<VertexName>,
        names: Vec<VertexName>,
    ) -> Result<Vec<(AncestorPath, Vec<VertexName>)>> {
        self.deref()
            .resolve_names_to_relative_paths(heads, names)
            .await
    }

    async fn resolve_relative_paths_to_names(
        &self,
        paths: Vec<AncestorPath>,
    ) -> Result<Vec<(AncestorPath, Vec<VertexName>)>> {
        self.deref().resolve_relative_paths_to_names(paths).await
    }
}

// Dag operations. Those are just simple wrappers around [`IdDag`].
// See [`IdDag`] for the actual implementations of these algorithms.

/// DAG related read-only algorithms.
#[async_trait::async_trait]
impl<IS, M, P, S> DagAlgorithm for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone + 'static,
    M: TryClone + IdConvert + Sync + Send + 'static,
    P: TryClone + Sync + Send + 'static,
    S: TryClone + Sync + Send + 'static,
{
    /// Sort a `NameSet` topologically.
    async fn sort(&self, set: &NameSet) -> Result<NameSet> {
        if set.hints().contains(Flags::TOPO_DESC)
            && set.hints().dag_version() <= Some(self.dag_version())
        {
            Ok(set.clone())
        } else {
            let flags = extract_ancestor_flag_if_compatible(set.hints(), self.dag_version());
            let mut spans = IdSet::empty();
            let mut iter = set.iter().await?.chunks(1 << 17);
            while let Some(names) = iter.next().await {
                let names = names.into_iter().collect::<Result<Vec<_>>>()?;
                let ids = self.vertex_id_batch(&names).await?;
                for id in ids {
                    spans.push(id?);
                }
            }
            let result = NameSet::from_spans_dag(spans, self)?;
            result.hints().add_flags(flags);
            Ok(result)
        }
    }

    /// Get ordered parent vertexes.
    async fn parent_names(&self, name: VertexName) -> Result<Vec<VertexName>> {
        let id = self.vertex_id(name).await?;
        let parent_ids = self.dag().parent_ids(id)?;
        let mut result = Vec::with_capacity(parent_ids.len());
        for id in parent_ids {
            result.push(self.vertex_name(id).await?);
        }
        Ok(result)
    }

    /// Returns a set that covers all vertexes tracked by this DAG.
    async fn all(&self) -> Result<NameSet> {
        let spans = self.dag().all()?;
        let result = NameSet::from_spans_dag(spans, self)?;
        result.hints().add_flags(Flags::FULL);
        Ok(result)
    }

    /// Returns a set that covers all vertexes in the master group.
    async fn master_group(&self) -> Result<NameSet> {
        let spans = self.dag().master_group()?;
        let result = NameSet::from_spans_dag(spans, self)?;
        result.hints().add_flags(Flags::ANCESTORS);
        Ok(result)
    }

    /// Calculates all ancestors reachable from any name from the given set.
    async fn ancestors(&self, set: NameSet) -> Result<NameSet> {
        if set.hints().contains(Flags::ANCESTORS)
            && set.hints().dag_version() <= Some(self.dag_version())
        {
            return Ok(set);
        }
        let spans = self.to_id_set(&set).await?;
        let spans = self.dag().ancestors(spans)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        result.hints().add_flags(Flags::ANCESTORS);
        Ok(result)
    }

    /// Like `ancestors` but follows only the first parents.
    async fn first_ancestors(&self, set: NameSet) -> Result<NameSet> {
        // If set == ancestors(set), then first_ancestors(set) == set.
        if set.hints().contains(Flags::ANCESTORS)
            && set.hints().dag_version() <= Some(self.dag_version())
        {
            return Ok(set);
        }
        let spans = self.to_id_set(&set).await?;
        let spans = self.dag().first_ancestors(spans)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        #[cfg(test)]
        {
            result.assert_eq(crate::default_impl::first_ancestors(self, set).await?);
        }
        Ok(result)
    }

    /// Calculate merges within the given set.
    async fn merges(&self, set: NameSet) -> Result<NameSet> {
        let spans = self.to_id_set(&set).await?;
        let spans = self.dag().merges(spans)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        #[cfg(test)]
        {
            result.assert_eq(crate::default_impl::merges(self, set).await?);
        }
        Ok(result)
    }

    /// Calculates parents of the given set.
    ///
    /// Note: Parent order is not preserved. Use [`NameDag::parent_names`]
    /// to preserve order.
    async fn parents(&self, set: NameSet) -> Result<NameSet> {
        // Preserve ANCESTORS flag. If ancestors(x) == x, then ancestors(parents(x)) == parents(x).
        let flags = extract_ancestor_flag_if_compatible(set.hints(), self.dag_version());
        let spans = self.dag().parents(self.to_id_set(&set).await?)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        result.hints().add_flags(flags);
        #[cfg(test)]
        {
            result.assert_eq(crate::default_impl::parents(self, set).await?);
        }
        Ok(result)
    }

    /// Calculates the n-th first ancestor.
    async fn first_ancestor_nth(&self, name: VertexName, n: u64) -> Result<Option<VertexName>> {
        #[cfg(test)]
        let name2 = name.clone();
        let id = self.vertex_id(name).await?;
        let id = self.dag().try_first_ancestor_nth(id, n)?;
        let result = match id {
            None => None,
            Some(id) => Some(self.vertex_name(id).await?),
        };
        #[cfg(test)]
        {
            let result2 = crate::default_impl::first_ancestor_nth(self, name2, n).await?;
            assert_eq!(result, result2);
        }
        Ok(result)
    }

    /// Calculates heads of the given set.
    async fn heads(&self, set: NameSet) -> Result<NameSet> {
        if set.hints().contains(Flags::ANCESTORS)
            && set.hints().dag_version() <= Some(self.dag_version())
        {
            // heads_ancestors is faster.
            return self.heads_ancestors(set).await;
        }
        let spans = self.dag().heads(self.to_id_set(&set).await?)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        #[cfg(test)]
        {
            result.assert_eq(crate::default_impl::heads(self, set).await?);
        }
        Ok(result)
    }

    /// Calculates children of the given set.
    async fn children(&self, set: NameSet) -> Result<NameSet> {
        let spans = self.dag().children(self.to_id_set(&set).await?)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        Ok(result)
    }

    /// Calculates roots of the given set.
    async fn roots(&self, set: NameSet) -> Result<NameSet> {
        let flags = extract_ancestor_flag_if_compatible(set.hints(), self.dag_version());
        let spans = self.dag().roots(self.to_id_set(&set).await?)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        result.hints().add_flags(flags);
        #[cfg(test)]
        {
            result.assert_eq(crate::default_impl::roots(self, set).await?);
        }
        Ok(result)
    }

    /// Calculates one "greatest common ancestor" of the given set.
    ///
    /// If there are no common ancestors, return None.
    /// If there are multiple greatest common ancestors, pick one arbitrarily.
    /// Use `gca_all` to get all of them.
    async fn gca_one(&self, set: NameSet) -> Result<Option<VertexName>> {
        let result: Option<VertexName> = match self.dag().gca_one(self.to_id_set(&set).await?)? {
            None => None,
            Some(id) => Some(self.vertex_name(id).await?),
        };
        #[cfg(test)]
        {
            assert_eq!(&result, &crate::default_impl::gca_one(self, set).await?);
        }
        Ok(result)
    }

    /// Calculates all "greatest common ancestor"s of the given set.
    /// `gca_one` is faster if an arbitrary answer is ok.
    async fn gca_all(&self, set: NameSet) -> Result<NameSet> {
        let spans = self.dag().gca_all(self.to_id_set(&set).await?)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        #[cfg(test)]
        {
            result.assert_eq(crate::default_impl::gca_all(self, set).await?);
        }
        Ok(result)
    }

    /// Calculates all common ancestors of the given set.
    async fn common_ancestors(&self, set: NameSet) -> Result<NameSet> {
        let spans = self.dag().common_ancestors(self.to_id_set(&set).await?)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        result.hints().add_flags(Flags::ANCESTORS);
        #[cfg(test)]
        {
            result.assert_eq(crate::default_impl::common_ancestors(self, set).await?);
        }
        Ok(result)
    }

    /// Tests if `ancestor` is an ancestor of `descendant`.
    async fn is_ancestor(&self, ancestor: VertexName, descendant: VertexName) -> Result<bool> {
        #[cfg(test)]
        let result2 =
            crate::default_impl::is_ancestor(self, ancestor.clone(), descendant.clone()).await?;
        let ancestor_id = self.vertex_id(ancestor).await?;
        let descendant_id = self.vertex_id(descendant).await?;
        let result = self.dag().is_ancestor(ancestor_id, descendant_id)?;
        #[cfg(test)]
        {
            assert_eq!(&result, &result2);
        }
        Ok(result)
    }

    /// Calculates "heads" of the ancestors of the given set. That is,
    /// Find Y, which is the smallest subset of set X, where `ancestors(Y)` is
    /// `ancestors(X)`.
    ///
    /// This is faster than calculating `heads(ancestors(set))`.
    ///
    /// This is different from `heads`. In case set contains X and Y, and Y is
    /// an ancestor of X, but not the immediate ancestor, `heads` will include
    /// Y while this function won't.
    async fn heads_ancestors(&self, set: NameSet) -> Result<NameSet> {
        let spans = self.dag().heads_ancestors(self.to_id_set(&set).await?)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        #[cfg(test)]
        {
            // default_impl::heads_ancestors calls `heads` if `Flags::ANCESTORS`
            // is set. Prevent infinite loop.
            if !set.hints().contains(Flags::ANCESTORS) {
                result.assert_eq(crate::default_impl::heads_ancestors(self, set).await?);
            }
        }
        Ok(result)
    }

    /// Calculates the "dag range" - vertexes reachable from both sides.
    async fn range(&self, roots: NameSet, heads: NameSet) -> Result<NameSet> {
        let roots = self.to_id_set(&roots).await?;
        let heads = self.to_id_set(&heads).await?;
        let spans = self.dag().range(roots, heads)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        Ok(result)
    }

    /// Calculates the descendants of the given set.
    async fn descendants(&self, set: NameSet) -> Result<NameSet> {
        let spans = self.dag().descendants(self.to_id_set(&set).await?)?;
        let result = NameSet::from_spans_dag(spans, self)?;
        Ok(result)
    }

    /// Vertexes buffered in memory, not yet written to disk.
    async fn dirty(&self) -> Result<NameSet> {
        let all = self.dag().all()?;
        let spans = all.difference(&self.persisted_id_set);
        let set = NameSet::from_spans_dag(spans, self)?;
        Ok(set)
    }

    fn is_vertex_lazy(&self) -> bool {
        !self.remote_protocol.is_local()
    }

    /// Get a snapshot of the current graph.
    fn dag_snapshot(&self) -> Result<Arc<dyn DagAlgorithm + Send + Sync>> {
        Ok(self.try_snapshot()? as Arc<dyn DagAlgorithm + Send + Sync>)
    }

    fn dag_id(&self) -> &str {
        &self.id
    }

    fn dag_version(&self) -> &VerLink {
        &self.dag.version()
    }
}

/// Extract the ANCESTORS flag if the set with the `hints` is bound to a
/// compatible DAG.
fn extract_ancestor_flag_if_compatible(hints: &Hints, dag_version: &VerLink) -> Flags {
    if hints.dag_version() <= Some(dag_version) {
        hints.flags() & Flags::ANCESTORS
    } else {
        Flags::empty()
    }
}

#[async_trait::async_trait]
impl<I, M, P, S> PrefixLookup for AbstractNameDag<I, M, P, S>
where
    I: Send + Sync,
    M: PrefixLookup + Send + Sync,
    P: Send + Sync,
    S: Send + Sync,
{
    async fn vertexes_by_hex_prefix(
        &self,
        hex_prefix: &[u8],
        limit: usize,
    ) -> Result<Vec<VertexName>> {
        let mut list = self.map.vertexes_by_hex_prefix(hex_prefix, limit).await?;
        let overlay_list = self
            .overlay_map
            .read()
            .lookup_vertexes_by_hex_prefix(hex_prefix, limit)?;
        list.extend(overlay_list);
        list.sort_unstable();
        list.dedup();
        list.truncate(limit);
        Ok(list)
    }
}

#[async_trait::async_trait]
impl<IS, M, P, S> IdConvert for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone,
    M: IdConvert + TryClone + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    async fn vertex_id(&self, name: VertexName) -> Result<Id> {
        match self.map.vertex_id(name.clone()).await {
            Ok(id) => Ok(id),
            Err(crate::Error::VertexNotFound(_)) if self.is_vertex_lazy() => {
                if let Some(id) = self.overlay_map.read().lookup_vertex_id(&name) {
                    return Ok(id);
                }
                if self
                    .missing_vertexes_confirmed_by_remote
                    .read()
                    .contains(&name)
                {
                    return name.not_found();
                }
                let ids = self.resolve_vertexes_remotely(&[name.clone()]).await?;
                if let Some(Some(id)) = ids.first() {
                    Ok(*id)
                } else {
                    // ids is empty.
                    name.not_found()
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn vertex_id_with_max_group(
        &self,
        name: &VertexName,
        max_group: Group,
    ) -> Result<Option<Id>> {
        match self.map.vertex_id_with_max_group(name, max_group).await {
            Ok(Some(id)) => Ok(Some(id)),
            Err(err) => Err(err),
            Ok(None) if self.is_vertex_lazy() => {
                if let Some(id) = self.overlay_map.read().lookup_vertex_id(&name) {
                    return Ok(Some(id));
                }
                if self
                    .missing_vertexes_confirmed_by_remote
                    .read()
                    .contains(&name)
                {
                    return Ok(None);
                }
                if max_group == Group::MASTER
                    && self
                        .map
                        .vertex_id_with_max_group(name, Group::NON_MASTER)
                        .await?
                        .is_some()
                {
                    // If the vertex exists in the non-master group. Then it must be missing in the
                    // master group.
                    return Ok(None);
                }
                match self.resolve_vertexes_remotely(&[name.clone()]).await {
                    Ok(ids) => match ids.first() {
                        Some(Some(id)) => Ok(Some(*id)),
                        Some(None) | None => Ok(None),
                    },
                    Err(e) => Err(e),
                }
            }
            Ok(None) => Ok(None),
        }
    }

    async fn vertex_name(&self, id: Id) -> Result<VertexName> {
        match self.map.vertex_name(id).await {
            Ok(name) => Ok(name),
            Err(crate::Error::IdNotFound(_)) if self.is_vertex_lazy() => {
                if let Some(name) = self.overlay_map.read().lookup_vertex_name(id) {
                    return Ok(name);
                }
                // Only ids <= max(MASTER group) can be lazy.
                let max_master_id = self.dag.master_group()?.max();
                if Some(id) > max_master_id {
                    return id.not_found();
                }
                let names = self.resolve_ids_remotely(&[id]).await?;
                if let Some(name) = names.into_iter().next() {
                    Ok(name)
                } else {
                    id.not_found()
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn contains_vertex_name(&self, name: &VertexName) -> Result<bool> {
        match self.map.contains_vertex_name(name).await {
            Ok(true) => Ok(true),
            Ok(false) if self.is_vertex_lazy() => {
                if self.overlay_map.read().lookup_vertex_id(name).is_some() {
                    return Ok(true);
                }
                if self
                    .missing_vertexes_confirmed_by_remote
                    .read()
                    .contains(&name)
                {
                    return Ok(false);
                }
                match self.resolve_vertexes_remotely(&[name.clone()]).await {
                    Ok(ids) => match ids.first() {
                        Some(Some(_)) => Ok(true),
                        Some(None) | None => Ok(false),
                    },
                    Err(e) => Err(e),
                }
            }
            Ok(false) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn contains_vertex_id_locally(&self, ids: &[Id]) -> Result<Vec<bool>> {
        let mut list = self.map.contains_vertex_id_locally(ids).await?;
        let map = self.overlay_map.read();
        for (b, id) in list.iter_mut().zip(ids.iter().copied()) {
            if !*b {
                *b = *b || map.has_vertex_id(id);
            }
        }
        Ok(list)
    }

    async fn contains_vertex_name_locally(&self, names: &[VertexName]) -> Result<Vec<bool>> {
        tracing::trace!("contains_vertex_name_locally names: {:?}", &names);
        let mut list = self.map.contains_vertex_name_locally(names).await?;
        tracing::trace!("contains_vertex_name_locally list (local): {:?}", &list);
        assert_eq!(list.len(), names.len());
        let map = self.overlay_map.read();
        for (b, name) in list.iter_mut().zip(names.iter()) {
            if !*b && map.has_vertex_name(name) {
                tracing::trace!("contains_vertex_name_locally overlay has {:?}", &name);
                *b = true;
            }
        }
        Ok(list)
    }

    async fn vertex_name_batch(&self, ids: &[Id]) -> Result<Vec<Result<VertexName>>> {
        let mut list = self.map.vertex_name_batch(ids).await?;
        if self.is_vertex_lazy() {
            // Read from overlay map cache.
            {
                let map = self.overlay_map.read();
                for (r, id) in list.iter_mut().zip(ids) {
                    if let Some(name) = map.lookup_vertex_name(*id) {
                        *r = Ok(name);
                    }
                }
            }
            // Read from missing_vertexes_confirmed_by_remote cache.
            let missing_indexes: Vec<usize> = {
                let max_master_id = self.dag.master_group()?.max();
                list.iter()
                    .enumerate()
                    .filter_map(|(i, r)| match r {
                        // Only resolve ids that are <= max(master) remotely.
                        Err(_) if Some(ids[i]) <= max_master_id => Some(i),
                        Err(_) | Ok(_) => None,
                    })
                    .collect()
            };
            let missing_ids: Vec<Id> = missing_indexes.iter().map(|i| ids[*i]).collect();
            let resolved = self.resolve_ids_remotely(&missing_ids).await?;
            for (i, name) in missing_indexes.into_iter().zip(resolved.into_iter()) {
                list[i] = Ok(name);
            }
        }
        Ok(list)
    }

    async fn vertex_id_batch(&self, names: &[VertexName]) -> Result<Vec<Result<Id>>> {
        let mut list = self.map.vertex_id_batch(names).await?;
        if self.is_vertex_lazy() {
            // Read from overlay map cache.
            {
                let map = self.overlay_map.read();
                for (r, name) in list.iter_mut().zip(names) {
                    if let Some(id) = map.lookup_vertex_id(name) {
                        *r = Ok(id);
                    }
                }
            }
            // Read from missing_vertexes_confirmed_by_remote cache.
            let missing_indexes: Vec<usize> = {
                let known_missing = self.missing_vertexes_confirmed_by_remote.read();
                list.iter()
                    .enumerate()
                    .filter_map(|(i, r)| {
                        if r.is_err() && !known_missing.contains(&names[i]) {
                            Some(i)
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            if !missing_indexes.is_empty() {
                let missing_names: Vec<VertexName> =
                    missing_indexes.iter().map(|i| names[*i].clone()).collect();
                let resolved = self.resolve_vertexes_remotely(&missing_names).await?;
                for (i, id) in missing_indexes.into_iter().zip(resolved.into_iter()) {
                    if let Some(id) = id {
                        list[i] = Ok(id);
                    }
                }
            }
        }
        Ok(list)
    }

    fn map_id(&self) -> &str {
        self.map.map_id()
    }

    fn map_version(&self) -> &VerLink {
        self.map.map_version()
    }
}

impl<IS, M, P, S> AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone + 'static,
    M: TryClone + IdMapAssignHead + IdConvert + Sync + Send + 'static,
    P: TryClone + Sync + Send + 'static,
    S: TryClone + Sync + Send + 'static,
{
    /// Export non-master DAG as parent_names_func on HashMap.
    ///
    /// This can be expensive. It is expected to be either called infrequently,
    /// or called with a small amount of data. For example, bounded amount of
    /// non-master commits.
    async fn non_master_parent_names(&self) -> Result<HashMap<VertexName, Vec<VertexName>>> {
        tracing::debug!(target: "dag::reassign", "calculating non-master subgraph");
        let parent_ids = self.dag.non_master_parent_ids()?;
        // PERF: This is suboptimal async iteration. It might be okay if non-master
        // part is not lazy.
        //
        // Map id to name.
        let mut parent_names_map = HashMap::with_capacity(parent_ids.len());
        for (id, parent_ids) in parent_ids.into_iter() {
            let name = self.vertex_name(id).await?;
            let parent_names = join_all(parent_ids.into_iter().map(|p| self.vertex_name(p)))
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            parent_names_map.insert(name, parent_names);
        }
        tracing::debug!(target: "dag::reassign", "non-master subgraph has {} entries", parent_names_map.len());
        Ok(parent_names_map)
    }

    /// Re-assign ids and segments for non-master group.
    fn rebuild_non_master<'a: 's, 's>(&'a mut self) -> BoxFuture<'s, Result<()>> {
        let fut = async move {
            // backup part of the named graph in memory.
            let parents = self.non_master_parent_names().await?;
            let mut heads = parents
                .keys()
                .collect::<HashSet<_>>()
                .difference(
                    &parents
                        .values()
                        .flat_map(|ps| ps.into_iter())
                        .collect::<HashSet<_>>(),
                )
                .map(|&v| v.clone())
                .collect::<Vec<_>>();
            heads.sort_unstable();
            tracing::debug!(target: "dag::reassign", "non-master heads: {} entries", heads.len());

            // Remove existing non-master data.
            self.dag.remove_non_master()?;
            self.map.remove_non_master().await?;

            // Populate vertex negative cache to reduce round-trips doing remote lookups.
            if self.is_vertex_lazy() {
                self.populate_missing_vertexes_for_add_heads(&parents, &heads)
                    .await?;
            }

            // Rebuild them.
            self.build(&parents, &[], &heads[..]).await?;

            Ok(())
        };
        Box::pin(fut)
    }

    /// Build IdMap and Segments for the given heads.
    async fn build(
        &mut self,
        parent_names_func: &dyn Parents,
        master_heads: &[VertexName],
        non_master_heads: &[VertexName],
    ) -> Result<()> {
        // Update IdMap.
        let mut outcome = PreparedFlatSegments::default();
        let mut covered = self.dag().all_ids_in_groups(&Group::ALL)?;
        let reserved = IdSet::empty();
        for (nodes, group) in [
            (master_heads, Group::MASTER),
            (non_master_heads, Group::NON_MASTER),
        ] {
            for node in nodes.iter() {
                // Important: do not call self.map.assign_head. It does not trigger
                // remote protocol properly.
                let prepared_segments = self
                    .assign_head(
                        node.clone(),
                        parent_names_func,
                        group,
                        &mut covered,
                        &reserved,
                    )
                    .await?;
                outcome.merge(prepared_segments);
            }
        }

        // Update segments.
        self.dag
            .build_segments_volatile_from_prepared_flat_segments(&outcome)?;

        // The master group might have new vertexes inserted, which will
        // affect the `overlay_map_next_id`.
        self.update_overlay_map_next_id()?;

        // Rebuild non-master ids and segments.
        if self.need_rebuild_non_master().await {
            self.rebuild_non_master().await?;
        }

        Ok(())
    }
}

fn is_ok_some<T>(value: Result<Option<T>>) -> bool {
    match value {
        Ok(Some(_)) => true,
        _ => false,
    }
}

impl<IS, M, P, S> IdMapSnapshot for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    IdDag<IS>: TryClone + 'static,
    M: TryClone + IdConvert + Send + Sync + 'static,
    P: TryClone + Send + Sync + 'static,
    S: TryClone + Send + Sync + 'static,
{
    fn id_map_snapshot(&self) -> Result<Arc<dyn IdConvert + Send + Sync>> {
        Ok(self.try_snapshot()? as Arc<dyn IdConvert + Send + Sync>)
    }
}

impl<IS, M, P, S> fmt::Debug for AbstractNameDag<IdDag<IS>, M, P, S>
where
    IS: IdDagStore,
    M: IdConvert + Send + Sync,
    P: Send + Sync,
    S: Send + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        debug(&self.dag, &self.map, f)
    }
}

pub(crate) fn debug_segments_by_level_group<S: IdDagStore>(
    iddag: &IdDag<S>,
    idmap: &dyn IdConvert,
    level: Level,
    group: Group,
) -> Vec<String> {
    let mut result = Vec::new();
    // Show Id, with optional hash.
    let show = |id: Id| DebugId {
        id,
        name: non_blocking_result(idmap.vertex_name(id)).ok(),
    };
    let show_flags = |flags: SegmentFlags| -> String {
        let mut result = Vec::new();
        if flags.contains(SegmentFlags::HAS_ROOT) {
            result.push("Root");
        }
        if flags.contains(SegmentFlags::ONLY_HEAD) {
            result.push("OnlyHead");
        }
        result.join(" ")
    };

    if let Ok(segments) = iddag.next_segments(group.min_id(), level) {
        for segment in segments.into_iter().rev() {
            if let (Ok(span), Ok(parents), Ok(flags)) =
                (segment.span(), segment.parents(), segment.flags())
            {
                let mut line = format!(
                    "{:.12?} : {:.12?} {:.12?}",
                    show(span.low),
                    show(span.high),
                    parents.into_iter().map(show).collect::<Vec<_>>(),
                );
                let flags = show_flags(flags);
                if !flags.is_empty() {
                    line += &format!(" {}", flags);
                }
                result.push(line);
            }
        }
    }
    result
}

fn debug<S: IdDagStore>(
    iddag: &IdDag<S>,
    idmap: &dyn IdConvert,
    f: &mut fmt::Formatter,
) -> fmt::Result {
    if let Ok(max_level) = iddag.max_level() {
        writeln!(f, "Max Level: {}", max_level)?;
        for lv in (0..=max_level).rev() {
            writeln!(f, " Level {}", lv)?;
            for group in Group::ALL.iter().cloned() {
                writeln!(f, "  {}:", group)?;
                if let Ok(id) = iddag.next_free_id(0, group) {
                    writeln!(f, "   Next Free Id: {}", id)?;
                }
                if let Ok(segments) = iddag.next_segments(group.min_id(), lv) {
                    writeln!(f, "   Segments: {}", segments.len())?;
                    for line in debug_segments_by_level_group(iddag, idmap, lv, group) {
                        writeln!(f, "    {}", line)?;
                    }
                }
            }
        }
    }

    Ok(())
}

struct DebugId {
    id: Id,
    name: Option<VertexName>,
}

impl fmt::Debug for DebugId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(name) = &self.name {
            fmt::Debug::fmt(&name, f)?;
            f.write_str("+")?;
        }
        write!(f, "{:?}", self.id)?;
        Ok(())
    }
}
