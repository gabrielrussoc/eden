/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Main function is `new_benchmark_repo` which creates `BlobRepo` which delay applied
//! to all underlying stores, but which all the caching enabled.

use anyhow::{anyhow, Error, Result};
use async_trait::async_trait;
use blobrepo::BlobRepo;
use blobstore::Blobstore;
use bonsai_git_mapping::{ArcBonsaiGitMapping, SqlBonsaiGitMappingConnection};
use bonsai_globalrev_mapping::{ArcBonsaiGlobalrevMapping, SqlBonsaiGlobalrevMapping};
use bonsai_hg_mapping::{
    ArcBonsaiHgMapping, BonsaiHgMapping, BonsaiHgMappingEntry, BonsaiOrHgChangesetIds,
    CachingBonsaiHgMapping, SqlBonsaiHgMappingBuilder,
};
use bonsai_svnrev_mapping::{
    ArcRepoBonsaiSvnrevMapping, RepoBonsaiSvnrevMapping, SqlBonsaiSvnrevMapping,
};
use bookmarks::{ArcBookmarkUpdateLog, ArcBookmarks};
use cacheblob::{dummy::DummyLease, new_cachelib_blobstore, CachelibBlobstoreOptions};
use changeset_fetcher::{ArcChangesetFetcher, SimpleChangesetFetcher};
use changesets::{ArcChangesets, ChangesetEntry, ChangesetInsert, Changesets, SortOrder};
use changesets_impl::{CachingChangesets, SqlChangesetsBuilder};
use context::CoreContext;
use dbbookmarks::{ArcSqlBookmarks, SqlBookmarksBuilder};
use delayblob::DelayedBlobstore;
use fbinit::FacebookInit;
use filenodes::{
    ArcFilenodes, FilenodeInfo, FilenodeRangeResult, FilenodeResult, Filenodes, PreparedFilenode,
};
use filestore::{ArcFilestoreConfig, FilestoreConfig};
use futures::stream::BoxStream;
use memblob::Memblob;
use mercurial_mutation::{ArcHgMutationStore, SqlHgMutationStoreBuilder};
use mercurial_types::{HgChangesetId, HgFileNodeId};
use metaconfig_types::ArcRepoConfig;
use mononoke_types::{
    ChangesetId, ChangesetIdPrefix, ChangesetIdsResolvedFromPrefix, RepoPath, RepositoryId,
};
use newfilenodes::NewFilenodesBuilder;
use phases::{ArcSqlPhasesFactory, SqlPhasesFactory};
use pushrebase_mutation_mapping::{
    ArcPushrebaseMutationMapping, SqlPushrebaseMutationMappingConnection,
};
use rand::Rng;
use rand_distr::Distribution;
use rendezvous::RendezVousOptions;
use repo_blobstore::{ArcRepoBlobstore, RepoBlobstore};
use repo_derived_data::{ArcRepoDerivedData, RepoDerivedData};
use repo_identity::{ArcRepoIdentity, RepoIdentity};
use scuba_ext::MononokeScubaSampleBuilder;
use segmented_changelog::DisabledSegmentedChangelog;
use segmented_changelog_types::ArcSegmentedChangelog;
use skiplist::{ArcSkiplistIndex, SkiplistIndex};
use sql_construct::SqlConstruct;
use std::{sync::Arc, time::Duration};

pub type Normal = rand_distr::Normal<f64>;

pub struct DelaySettings {
    pub blobstore_put_dist: Normal,
    pub blobstore_get_dist: Normal,
    pub db_put_dist: Normal,
    pub db_get_dist: Normal,
}

impl Default for DelaySettings {
    fn default() -> Self {
        Self {
            blobstore_put_dist: Normal::new(0.1, 0.05).expect("Normal::new failed"),
            blobstore_get_dist: Normal::new(0.05, 0.025).expect("Normal::new failed"),
            db_put_dist: Normal::new(0.02, 0.01).expect("Normal::new failed"),
            db_get_dist: Normal::new(0.02, 0.01).expect("Normal::new failed"),
        }
    }
}

pub struct BenchmarkRepoFactory {
    fb: FacebookInit,
    delay_settings: DelaySettings,
}

impl BenchmarkRepoFactory {
    pub fn new(fb: FacebookInit, delay_settings: DelaySettings) -> Self {
        BenchmarkRepoFactory { fb, delay_settings }
    }
}

fn cache_pool(name: &str) -> Result<cachelib::LruCachePool> {
    Ok(cachelib::get_pool(name).ok_or_else(|| anyhow!("no cache pool: {}", name))?)
}

fn volatile_pool(name: &str) -> Result<cachelib::VolatileLruCachePool> {
    Ok(cachelib::get_volatile_pool(name)?.ok_or_else(|| anyhow!("no cache pool: {}", name))?)
}

#[facet::factory()]
impl BenchmarkRepoFactory {
    pub fn repo_blobstore(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcRepoBlobstore> {
        let blobstore: Arc<dyn Blobstore> = Arc::new(DelayedBlobstore::new(
            Memblob::default(),
            self.delay_settings.blobstore_get_dist,
            self.delay_settings.blobstore_put_dist,
        ));
        let blobstore = Arc::new(new_cachelib_blobstore(
            blobstore,
            Arc::new(cache_pool("blobstore-blobs")?),
            Arc::new(cache_pool("blobstore-presence")?),
            CachelibBlobstoreOptions::default(),
        ));
        let repo_blobstore = RepoBlobstore::new(
            blobstore,
            None,
            repo_identity.id(),
            MononokeScubaSampleBuilder::with_discard(),
        );
        Ok(Arc::new(repo_blobstore))
    }

    pub fn repo_config(&self, repo_identity: &ArcRepoIdentity) -> ArcRepoConfig {
        let mut config = test_repo_factory::default_test_repo_config();
        config.repoid = repo_identity.id();
        Arc::new(config)
    }

    pub fn repo_identity(&self) -> ArcRepoIdentity {
        Arc::new(RepoIdentity::new(
            RepositoryId::new(rand::random()),
            "benchmarkrepo".to_string(),
        ))
    }

    pub fn changesets(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcChangesets> {
        let changesets: Arc<dyn Changesets> = Arc::new(DelayedChangesets::new(
            SqlChangesetsBuilder::with_sqlite_in_memory()?
                .build(RendezVousOptions::for_test(), repo_identity.id()),
            self.delay_settings.db_get_dist,
            self.delay_settings.db_put_dist,
        ));
        Ok(Arc::new(CachingChangesets::new(
            self.fb,
            changesets,
            volatile_pool("changesets")?,
        )))
    }

    pub fn changeset_fetcher(
        &self,
        repo_identity: &ArcRepoIdentity,
        changesets: &ArcChangesets,
    ) -> ArcChangesetFetcher {
        Arc::new(SimpleChangesetFetcher::new(
            changesets.clone(),
            repo_identity.id(),
        ))
    }

    pub fn sql_bookmarks(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcSqlBookmarks> {
        // TODO:
        //  - add caching
        //  - add delay
        Ok(Arc::new(
            SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(repo_identity.id()),
        ))
    }

    pub fn bookmarks(&self, sql_bookmarks: &ArcSqlBookmarks) -> ArcBookmarks {
        sql_bookmarks.clone()
    }

    pub fn bookmark_update_log(&self, sql_bookmarks: &ArcSqlBookmarks) -> ArcBookmarkUpdateLog {
        sql_bookmarks.clone()
    }

    pub fn sql_phases_factory(&self) -> Result<ArcSqlPhasesFactory> {
        Ok(Arc::new(SqlPhasesFactory::with_sqlite_in_memory()?))
    }

    pub fn bonsai_hg_mapping(&self) -> Result<ArcBonsaiHgMapping> {
        let mapping: Arc<dyn BonsaiHgMapping> = Arc::new(DelayedBonsaiHgMapping::new(
            SqlBonsaiHgMappingBuilder::with_sqlite_in_memory()?
                .build(RendezVousOptions::for_test()),
            self.delay_settings.db_get_dist,
            self.delay_settings.db_put_dist,
        ));
        Ok(Arc::new(CachingBonsaiHgMapping::new(
            self.fb,
            mapping,
            volatile_pool("bonsai_hg_mapping")?,
        )))
    }

    pub fn bonsai_git_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcBonsaiGitMapping> {
        Ok(Arc::new(
            SqlBonsaiGitMappingConnection::with_sqlite_in_memory()?
                .with_repo_id(repo_identity.id()),
        ))
    }

    pub fn bonsai_globalrev_mapping(&self) -> Result<ArcBonsaiGlobalrevMapping> {
        Ok(Arc::new(SqlBonsaiGlobalrevMapping::with_sqlite_in_memory()?))
    }

    pub fn pushrebase_mutation_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcPushrebaseMutationMapping> {
        Ok(Arc::new(
            SqlPushrebaseMutationMappingConnection::with_sqlite_in_memory()?
                .with_repo_id(repo_identity.id()),
        ))
    }

    pub fn repo_bonsai_svnrev_mapping(
        &self,
        repo_identity: &ArcRepoIdentity,
    ) -> Result<ArcRepoBonsaiSvnrevMapping> {
        Ok(Arc::new(RepoBonsaiSvnrevMapping::new(
            repo_identity.id(),
            Arc::new(SqlBonsaiSvnrevMapping::with_sqlite_in_memory()?),
        )))
    }

    pub fn filenodes(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcFilenodes> {
        let pool = volatile_pool("filenodes")?;

        let mut builder = NewFilenodesBuilder::with_sqlite_in_memory()?;
        builder.enable_caching(self.fb, pool.clone(), pool, "filenodes", "");

        Ok(Arc::new(DelayedFilenodes::new(
            builder.build(repo_identity.id()),
            self.delay_settings.db_get_dist,
            self.delay_settings.db_put_dist,
        )))
    }

    pub fn hg_mutation_store(&self, repo_identity: &ArcRepoIdentity) -> Result<ArcHgMutationStore> {
        Ok(Arc::new(
            SqlHgMutationStoreBuilder::with_sqlite_in_memory()?.with_repo_id(repo_identity.id()),
        ))
    }

    pub fn segmented_changelog(&self) -> ArcSegmentedChangelog {
        Arc::new(DisabledSegmentedChangelog::new())
    }

    pub fn repo_derived_data(
        &self,
        repo_identity: &ArcRepoIdentity,
        repo_config: &ArcRepoConfig,
        changesets: &ArcChangesets,
        bonsai_hg_mapping: &ArcBonsaiHgMapping,
        filenodes: &ArcFilenodes,
        repo_blobstore: &ArcRepoBlobstore,
    ) -> Result<ArcRepoDerivedData> {
        Ok(Arc::new(RepoDerivedData::new(
            repo_identity.id(),
            repo_identity.name().to_string(),
            changesets.clone(),
            bonsai_hg_mapping.clone(),
            filenodes.clone(),
            repo_blobstore.as_ref().clone(),
            Arc::new(DummyLease {}),
            MononokeScubaSampleBuilder::with_discard(),
            repo_config.derived_data_config.clone(),
            None, // derivation_service_client = None
        )?))
    }

    pub fn filestore_config(&self) -> ArcFilestoreConfig {
        Arc::new(FilestoreConfig::default())
    }

    pub fn skiplist_index(&self) -> ArcSkiplistIndex {
        Arc::new(SkiplistIndex::new())
    }
}

pub fn new_benchmark_repo(fb: FacebookInit, settings: DelaySettings) -> Result<BlobRepo> {
    let repo = BenchmarkRepoFactory::new(fb, settings).build()?;
    Ok(repo)
}

/// Delay target future execution by delay sampled from provided distribution
async fn delay(distribution: impl Distribution<f64>) {
    let seconds = rand::thread_rng().sample(distribution).abs();
    let duration = Duration::from_secs_f64(seconds);
    tokio::time::sleep(duration).await;
}

struct DelayedFilenodes<F> {
    inner: F,
    get_dist: Normal,
    put_dist: Normal,
}

impl<F> DelayedFilenodes<F> {
    fn new(inner: F, get_dist: Normal, put_dist: Normal) -> Self {
        Self {
            inner,
            get_dist,
            put_dist,
        }
    }
}

#[async_trait]
impl<F: Filenodes> Filenodes for DelayedFilenodes<F> {
    async fn add_filenodes(
        &self,
        ctx: &CoreContext,
        info: Vec<PreparedFilenode>,
    ) -> Result<FilenodeResult<()>> {
        delay(self.put_dist).await;
        self.inner.add_filenodes(ctx, info).await
    }

    async fn add_or_replace_filenodes(
        &self,
        ctx: &CoreContext,
        info: Vec<PreparedFilenode>,
    ) -> Result<FilenodeResult<()>> {
        delay(self.put_dist).await;
        self.inner.add_or_replace_filenodes(ctx, info).await
    }

    async fn get_filenode(
        &self,
        ctx: &CoreContext,
        path: &RepoPath,
        filenode: HgFileNodeId,
    ) -> Result<FilenodeResult<Option<FilenodeInfo>>> {
        delay(self.get_dist).await;
        self.inner.get_filenode(ctx, path, filenode).await
    }

    async fn get_all_filenodes_maybe_stale(
        &self,
        ctx: &CoreContext,
        path: &RepoPath,
        limit: Option<u64>,
    ) -> Result<FilenodeRangeResult<Vec<FilenodeInfo>>> {
        delay(self.get_dist).await;
        self.inner
            .get_all_filenodes_maybe_stale(ctx, path, limit)
            .await
    }

    fn prime_cache(&self, ctx: &CoreContext, filenodes: &[PreparedFilenode]) {
        self.inner.prime_cache(ctx, filenodes)
    }
}

struct DelayedChangesets<C> {
    inner: C,
    get_dist: Normal,
    put_dist: Normal,
}

impl<C> DelayedChangesets<C> {
    fn new(inner: C, get_dist: Normal, put_dist: Normal) -> Self {
        Self {
            inner,
            get_dist,
            put_dist,
        }
    }
}

#[async_trait]
impl<C: Changesets> Changesets for DelayedChangesets<C> {
    fn repo_id(&self) -> RepositoryId {
        self.inner.repo_id()
    }

    async fn add(&self, ctx: CoreContext, cs: ChangesetInsert) -> Result<bool, Error> {
        delay(self.put_dist).await;
        self.inner.add(ctx, cs).await
    }

    async fn get(
        &self,
        ctx: CoreContext,
        cs_id: ChangesetId,
    ) -> Result<Option<ChangesetEntry>, Error> {
        delay(self.get_dist).await;
        self.inner.get(ctx, cs_id).await
    }

    async fn get_many(
        &self,
        ctx: CoreContext,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<Vec<ChangesetEntry>, Error> {
        delay(self.get_dist).await;
        self.inner.get_many(ctx, cs_ids).await
    }

    async fn get_many_by_prefix(
        &self,
        ctx: CoreContext,
        cs_prefix: ChangesetIdPrefix,
        limit: usize,
    ) -> Result<ChangesetIdsResolvedFromPrefix, Error> {
        delay(self.get_dist).await;
        self.inner.get_many_by_prefix(ctx, cs_prefix, limit).await
    }

    fn prime_cache(&self, ctx: &CoreContext, changesets: &[ChangesetEntry]) {
        self.inner.prime_cache(ctx, changesets)
    }

    async fn enumeration_bounds(
        &self,
        ctx: &CoreContext,
        read_from_master: bool,
    ) -> Result<Option<(u64, u64)>, Error> {
        self.inner.enumeration_bounds(ctx, read_from_master).await
    }

    fn list_enumeration_range(
        &self,
        ctx: &CoreContext,
        min_id: u64,
        max_id: u64,
        sort_and_limit: Option<(SortOrder, u64)>,
        read_from_master: bool,
    ) -> BoxStream<'_, Result<(ChangesetId, u64), Error>> {
        self.inner
            .list_enumeration_range(ctx, min_id, max_id, sort_and_limit, read_from_master)
    }
}

struct DelayedBonsaiHgMapping<M> {
    inner: M,
    get_dist: Normal,
    put_dist: Normal,
}

impl<M> DelayedBonsaiHgMapping<M> {
    fn new(inner: M, get_dist: Normal, put_dist: Normal) -> Self {
        Self {
            inner,
            get_dist,
            put_dist,
        }
    }
}

#[async_trait]
impl<M: BonsaiHgMapping> BonsaiHgMapping for DelayedBonsaiHgMapping<M> {
    async fn add(&self, ctx: &CoreContext, entry: BonsaiHgMappingEntry) -> Result<bool, Error> {
        delay(self.put_dist).await;
        self.inner.add(ctx, entry).await
    }

    async fn get(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        cs_id: BonsaiOrHgChangesetIds,
    ) -> Result<Vec<BonsaiHgMappingEntry>, Error> {
        delay(self.get_dist).await;
        self.inner.get(ctx, repo_id, cs_id).await
    }

    async fn get_hg_in_range(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        low: HgChangesetId,
        high: HgChangesetId,
        limit: usize,
    ) -> Result<Vec<HgChangesetId>, Error> {
        delay(self.get_dist).await;
        self.inner
            .get_hg_in_range(ctx, repo_id, low, high, limit)
            .await
    }
}
