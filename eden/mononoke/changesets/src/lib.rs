/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]

use anyhow::{Error, Result};
use async_trait::async_trait;
use auto_impl::auto_impl;
use context::CoreContext;
use futures::stream::BoxStream;
use mononoke_types::{
    ChangesetId, ChangesetIdPrefix, ChangesetIdsResolvedFromPrefix, RepositoryId,
};

mod entry;

pub use crate::entry::{deserialize_cs_entries, serialize_cs_entries, ChangesetEntry};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChangesetInsert {
    pub cs_id: ChangesetId,
    pub parents: Vec<ChangesetId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

/// Interface to storage of changesets that have been completely stored in Mononoke.
#[facet::facet]
#[async_trait]
#[auto_impl(&, Arc)]
pub trait Changesets: Send + Sync {
    /// The repository this `Changesets` is for.
    fn repo_id(&self) -> RepositoryId;

    /// Add a new entry to the changesets table. Returns true if new changeset was inserted,
    /// returns false if the same changeset has already existed.
    async fn add(&self, ctx: CoreContext, cs: ChangesetInsert) -> Result<bool, Error>;

    /// Retrieve the row specified by this commit, if available.
    async fn get(
        &self,
        ctx: CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Option<ChangesetEntry>, Error>;

    /// Return whether a changeset is stored in the backend
    async fn exists(&self, ctx: &CoreContext, cs_id: ChangesetId) -> Result<bool, Error> {
        Ok(self.get(ctx.clone(), cs_id).await?.is_some())
    }

    /// Retrieve the rows for all the commits if available
    async fn get_many(
        &self,
        ctx: CoreContext,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetEntry>, Error>;

    /// Retrieve the rows for all the commits with the given prefix up to the given limit
    async fn get_many_by_prefix(
        &self,
        ctx: CoreContext,
        cs_prefix: ChangesetIdPrefix,
        limit: usize,
    ) -> Result<ChangesetIdsResolvedFromPrefix, Error>;

    /// Prime any caches with known changeset entries.  The changeset entries
    /// must be for the repository associated with this `Changesets`.
    fn prime_cache(&self, ctx: &CoreContext, changesets: &[ChangesetEntry]);

    /// Enumerate all public changesets in the repository.
    ///
    /// This returns a pair of unique integers that are the minimum and
    /// maximum unique changeset ids for this repository.
    ///
    /// This range can be used in subsequent calls to `list_enumeration_range`
    /// to enumerate the changesets.
    async fn enumeration_bounds(
        &self,
        ctx: &CoreContext,
        read_from_master: bool,
    ) -> Result<Option<(u64, u64)>>;

    /// Enumerate a range of public changesets in the repository.
    ///
    /// This lists all changesets in the given range of unique integer ids
    /// that belong to this repositories, along with their unique integer ids.
    /// Unique ids are assigned for all changesets (public or draft) in all
    /// repositories, so a given range may not have any changesets for this
    /// repository.
    ///
    /// The results can optionally be sorted and limited so that enumeration
    /// can be performed in chunks for repositories with large numbers of
    /// commits.
    ///
    /// Use `enumeration_bounds` to find suitable starting values for
    /// `min_id` and `max_id`.
    fn list_enumeration_range(
        &self,
        ctx: &CoreContext,
        min_id: u64,
        max_id: u64,
        sort_and_limit: Option<(SortOrder, u64)>,
        read_from_master: bool,
    ) -> BoxStream<'_, Result<(ChangesetId, u64), Error>>;
}
