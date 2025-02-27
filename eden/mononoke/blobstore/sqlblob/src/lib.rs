/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]

mod delay;
#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod myadmin_delay_dummy;
mod store;
#[cfg(test)]
mod tests;

use crate::delay::BlobDelay;
#[cfg(fbcode_build)]
use crate::facebook::myadmin_delay;
#[cfg(not(fbcode_build))]
use crate::myadmin_delay_dummy as myadmin_delay;
use crate::store::{ChunkSqlStore, ChunkingMethod, DataSqlStore};
use anyhow::{bail, format_err, Error, Result};
use async_trait::async_trait;
use blobstore::{
    Blobstore, BlobstoreGetData, BlobstoreIsPresent, BlobstoreMetadata, BlobstorePutOps,
    BlobstoreWithLink, CountedBlobstore, OverwriteStatus, PutBehaviour,
};
use bytes::{Bytes, BytesMut};
use cached_config::{ConfigHandle, ConfigStore, ModificationTime, TestSource};
use context::CoreContext;
use fbinit::FacebookInit;
use futures::stream::{FuturesOrdered, FuturesUnordered, Stream, TryStreamExt};
use mononoke_types::{hash::Context as HashContext, BlobstoreBytes};
use nonzero_ext::nonzero;
use sql::{rusqlite::Connection as SqliteConnection, Connection};
use sql_ext::{
    facebook::{
        create_mysql_connections_sharded, create_mysql_connections_unsharded, MysqlOptions,
    },
    open_sqlite_in_memory, open_sqlite_path, SqlConnections, SqlShardedConnections,
};
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    num::NonZeroUsize,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::task::spawn_blocking;
use xdb_gc_structs::XdbGc;

// Leaving some space for metadata
const MAX_KEY_SIZE: usize = 200;
// MySQL wants multiple chunks, each around 1 MiB, as a tradeoff between query latency and replication lag
const CHUNK_SIZE: usize = 1024 * 1024;
const SQLITE_SHARD_NUM: NonZeroUsize = nonzero!(2_usize);
const SINGLE_SHARD_NUM: NonZeroUsize = nonzero!(1_usize);
const GC_GENERATION_PATH: &str = "scm/mononoke/xdb_gc/default";

const SQLBLOB_LABEL: &str = "blobstore";

// Test setup data
const UPDATE_FREQUENCY: Duration = Duration::from_millis(1);
const INITIAL_VERSION: u64 = 0;

const COUNTED_ID: &str = "sqlblob";
pub type CountedSqlblob = CountedBlobstore<Sqlblob>;

pub struct Sqlblob {
    data_store: Arc<DataSqlStore>,
    chunk_store: Arc<ChunkSqlStore>,
    put_behaviour: PutBehaviour,
    allow_inline_put: bool,
}

impl std::fmt::Display for Sqlblob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sqlblob")
    }
}

fn get_gc_config_handle(config_store: &ConfigStore) -> Result<ConfigHandle<XdbGc>> {
    config_store.get_config_handle(GC_GENERATION_PATH.to_string())
}

const DEFAULT_ALLOW_INLINE_PUT: bool = true;

// base64 encoding for inline hash has an overhead
pub const MAX_INLINE_LEN: usize = 255 * 3 / 4;

impl Sqlblob {
    pub async fn with_mysql(
        fb: FacebookInit,
        shardmap: String,
        shard_num: NonZeroUsize,
        mysql_options: MysqlOptions,
        readonly: bool,
        put_behaviour: PutBehaviour,
        config_store: &ConfigStore,
    ) -> Result<CountedSqlblob, Error> {
        let delay = if readonly {
            BlobDelay::dummy(shard_num)
        } else {
            myadmin_delay::sharded(fb, shardmap.clone(), shard_num)?
        };
        let config_handle = get_gc_config_handle(config_store)?;
        let shard_count = shard_num.clone().get();

        let SqlShardedConnections {
            read_connections,
            read_master_connections,
            write_connections,
        } = spawn_blocking({
            let shardmap = shardmap.clone();
            move || {
                create_mysql_connections_sharded(
                    fb,
                    mysql_options,
                    SQLBLOB_LABEL.into(),
                    shardmap,
                    0..shard_count,
                    readonly,
                )
            }
        })
        .await??;

        let write_connections = Arc::new(write_connections);
        let read_connections = Arc::new(read_connections);
        let read_master_connections = Arc::new(read_master_connections);
        Ok(Self::counted(
            Self {
                data_store: Arc::new(DataSqlStore::new(
                    shard_num,
                    write_connections.clone(),
                    read_connections.clone(),
                    read_master_connections.clone(),
                    delay.clone(),
                )),
                chunk_store: Arc::new(ChunkSqlStore::new(
                    shard_num,
                    write_connections,
                    read_connections,
                    read_master_connections,
                    delay,
                    config_handle,
                )),
                put_behaviour,
                allow_inline_put: DEFAULT_ALLOW_INLINE_PUT,
            },
            shardmap,
        ))
    }

    pub async fn with_mysql_unsharded(
        fb: FacebookInit,
        db_address: String,
        mysql_options: MysqlOptions,
        readonly: bool,
        put_behaviour: PutBehaviour,
        config_store: &ConfigStore,
    ) -> Result<CountedSqlblob, Error> {
        let delay = if readonly {
            BlobDelay::dummy(SINGLE_SHARD_NUM)
        } else {
            myadmin_delay::single(fb, db_address.clone())?
        };
        Self::with_connection_factory(
            delay,
            db_address.clone(),
            SINGLE_SHARD_NUM,
            put_behaviour,
            move |_shard_id| {
                let res = create_mysql_connections_unsharded(
                    fb,
                    mysql_options.clone(),
                    SQLBLOB_LABEL.into(),
                    db_address.clone(),
                    readonly,
                );
                async { res }
            },
            config_store,
            DEFAULT_ALLOW_INLINE_PUT,
        )
        .await
    }

    async fn with_connection_factory<CF, SF>(
        delay: BlobDelay,
        label: String,
        shard_num: NonZeroUsize,
        put_behaviour: PutBehaviour,
        connection_factory: CF,
        config_store: &ConfigStore,
        allow_inline_put: bool,
    ) -> Result<CountedSqlblob, Error>
    where
        CF: Fn(usize) -> SF,
        SF: Future<Output = Result<SqlConnections, Error>> + Sized,
    {
        let shard_count = shard_num.get();

        let config_handle = get_gc_config_handle(config_store)?;

        let futs: FuturesOrdered<_> = (0..shard_count)
            .into_iter()
            .map(|shard| connection_factory(shard))
            .collect();

        let shard_connections = futs.try_collect::<Vec<_>>().await?;
        let mut write_connections = Vec::with_capacity(shard_count);
        let mut read_connections = Vec::with_capacity(shard_count);
        let mut read_master_connections = Vec::with_capacity(shard_count);

        for connections in shard_connections {
            write_connections.push(connections.write_connection);
            read_connections.push(connections.read_connection);
            read_master_connections.push(connections.read_master_connection);
        }

        let write_connections = Arc::new(write_connections);
        let read_connections = Arc::new(read_connections);
        let read_master_connections = Arc::new(read_master_connections);

        Ok(Self::counted(
            Self {
                data_store: Arc::new(DataSqlStore::new(
                    shard_num,
                    write_connections.clone(),
                    read_connections.clone(),
                    read_master_connections.clone(),
                    delay.clone(),
                )),
                chunk_store: Arc::new(ChunkSqlStore::new(
                    shard_num,
                    write_connections,
                    read_connections,
                    read_master_connections,
                    delay,
                    config_handle,
                )),
                put_behaviour,
                allow_inline_put,
            },
            label,
        ))
    }

    pub fn with_sqlite_in_memory(
        put_behaviour: PutBehaviour,
        config_store: &ConfigStore,
        allow_inline_put: bool,
    ) -> Result<CountedSqlblob> {
        Self::with_sqlite(
            put_behaviour,
            |_| {
                let con = open_sqlite_in_memory()?;
                con.execute_batch(Self::CREATION_QUERY)?;
                Ok(con)
            },
            config_store,
            allow_inline_put,
        )
    }

    pub fn with_sqlite_path<P: Into<PathBuf>>(
        path: P,
        readonly_storage: bool,
        put_behaviour: PutBehaviour,
        config_store: &ConfigStore,
    ) -> Result<CountedSqlblob> {
        let pathbuf = path.into();
        Self::with_sqlite(
            put_behaviour,
            move |shard_id| {
                let con = open_sqlite_path(
                    &pathbuf.join(format!("shard_{}.sqlite", shard_id)),
                    readonly_storage,
                )?;
                con.execute_batch(Self::CREATION_QUERY)?;
                Ok(con)
            },
            config_store,
            DEFAULT_ALLOW_INLINE_PUT,
        )
    }

    fn with_sqlite<F>(
        put_behaviour: PutBehaviour,
        mut constructor: F,
        config_store: &ConfigStore,
        allow_inline_put: bool,
    ) -> Result<CountedSqlblob>
    where
        F: FnMut(usize) -> Result<SqliteConnection>,
    {
        let mut cons = Vec::new();

        for i in 0..SQLITE_SHARD_NUM.get() {
            cons.push(Connection::with_sqlite(constructor(i)?));
        }

        let cons = Arc::new(cons);

        // SQLite is predominately intended for tests, and has less concurrency
        // issues relating to GC, so cope with missing configerator
        let config_handle = get_gc_config_handle(config_store)
            .or_else(|_| get_gc_config_handle(&(get_test_config_store().1)))?;

        Ok(Self::counted(
            Self {
                data_store: Arc::new(DataSqlStore::new(
                    SQLITE_SHARD_NUM,
                    cons.clone(),
                    cons.clone(),
                    cons.clone(),
                    BlobDelay::dummy(SQLITE_SHARD_NUM),
                )),
                chunk_store: Arc::new(ChunkSqlStore::new(
                    SQLITE_SHARD_NUM,
                    cons.clone(),
                    cons.clone(),
                    cons,
                    BlobDelay::dummy(SQLITE_SHARD_NUM),
                    config_handle,
                )),
                put_behaviour,
                allow_inline_put,
            },
            "sqlite".into(),
        ))
    }

    const CREATION_QUERY: &'static str = include_str!("../schema/sqlite-sqlblob.sql");

    fn counted(self, label: String) -> CountedBlobstore<Self> {
        CountedBlobstore::new(format!("{}.{}", COUNTED_ID, label), self)
    }

    #[cfg(test)]
    pub(crate) fn get_data_store(&self) -> &DataSqlStore {
        &self.data_store
    }

    pub fn get_keys_from_shard(&self, shard_num: usize) -> impl Stream<Item = Result<String>> {
        self.data_store.get_keys_from_shard(shard_num)
    }

    pub async fn get_chunk_sizes_by_generation(
        &self,
        shard_num: usize,
    ) -> Result<HashMap<Option<u64>, u64>> {
        self.chunk_store
            .get_chunk_sizes_by_generation(shard_num)
            .await
    }

    pub async fn set_initial_generation(&self, shard_num: usize) -> Result<()> {
        self.chunk_store.set_initial_generation(shard_num).await
    }

    #[cfg(test)]
    pub async fn get_chunk_generations(&self, key: &str) -> Result<Vec<Option<u64>>> {
        let chunked = self.data_store.get(key).await?;
        if let Some(chunked) = chunked {
            let fetch_chunk_generations: FuturesOrdered<_> = (0..chunked.count)
                .map(|chunk_num| {
                    self.chunk_store
                        .get_generation(&chunked.id, chunk_num, chunked.chunking_method)
                })
                .collect();
            fetch_chunk_generations.try_collect().await
        } else {
            bail!("key does not exist");
        }
    }

    pub async fn set_generation(&self, key: &str) -> Result<()> {
        let chunked = self.data_store.get(key).await?;
        if let Some(chunked) = chunked {
            let set_chunk_generations: FuturesUnordered<_> = (0..chunked.count)
                .map(|chunk_num| {
                    self.chunk_store
                        .set_generation(&chunked.id, chunk_num, chunked.chunking_method)
                })
                .collect();
            set_chunk_generations.try_collect().await
        } else {
            bail!("key does not exist");
        }
    }
}

impl fmt::Debug for Sqlblob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sqlblob").finish()
    }
}

#[async_trait]
impl Blobstore for Sqlblob {
    async fn get<'a>(
        &'a self,
        _ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        let chunked = self.data_store.get(&key).await?;
        if let Some(chunked) = chunked {
            let blob = match chunked.chunking_method {
                ChunkingMethod::InlineBase64 => {
                    let decoded = base64::decode_config(&chunked.id, base64::STANDARD_NO_PAD)?;
                    Bytes::copy_from_slice(decoded.as_ref())
                }
                ChunkingMethod::ByContentHashBlake2 => {
                    let chunks = (0..chunked.count)
                        .map(|chunk_num| {
                            self.chunk_store
                                .get(&chunked.id, chunk_num, chunked.chunking_method)
                        })
                        .collect::<FuturesOrdered<_>>()
                        .try_collect::<Vec<_>>()
                        .await?;

                    let size = chunks.iter().map(|chunk| chunk.len()).sum();
                    let mut blob = BytesMut::with_capacity(size);
                    for chunk in chunks {
                        blob.extend_from_slice(&chunk);
                    }
                    blob.freeze()
                }
            };

            let meta = BlobstoreMetadata::new(Some(chunked.ctime), None);
            Ok(Some(BlobstoreGetData::new(
                meta,
                BlobstoreBytes::from_bytes(blob),
            )))
        } else {
            Ok(None)
        }
    }

    async fn is_present<'a>(
        &'a self,
        _ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        let present = self.data_store.is_present(&key).await?;
        Ok(if present {
            BlobstoreIsPresent::Present
        } else {
            BlobstoreIsPresent::Absent
        })
    }

    async fn put<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<()> {
        BlobstorePutOps::put_with_status(self, ctx, key, value).await?;
        Ok(())
    }
}

#[async_trait]
impl BlobstorePutOps for Sqlblob {
    async fn put_explicit<'a>(
        &'a self,
        _ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        if key.as_bytes().len() > MAX_KEY_SIZE {
            return Err(format_err!(
                "Key {} exceeded max key size {}",
                key,
                MAX_KEY_SIZE
            ));
        }

        if put_behaviour == PutBehaviour::IfAbsent && self.data_store.is_present(&key).await? {
            // Can short circuit here as key already exists, and is keeping its chunks live
            return Ok(OverwriteStatus::Prevented);
        }

        let chunking_method = if self.allow_inline_put && value.len() <= MAX_INLINE_LEN {
            ChunkingMethod::InlineBase64
        } else {
            ChunkingMethod::ByContentHashBlake2
        };

        let put_fut = async {
            let ctime = {
                match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(offset) => offset.as_secs().try_into(),
                    Err(negative) => negative.duration().as_secs().try_into().map(|v: i64| -v),
                }
            }?;
            let (chunk_key, chunk_count) = match chunking_method {
                ChunkingMethod::ByContentHashBlake2 => {
                    let chunk_key = {
                        let mut hash_context = HashContext::new(b"sqlblob");
                        hash_context.update(value.as_bytes());
                        hash_context.finish().to_hex().to_string()
                    };
                    let chunks = value.as_bytes().chunks(CHUNK_SIZE);
                    let chunk_count = chunks.len().try_into()?;
                    for (chunk_num, value) in chunks.enumerate() {
                        self.chunk_store
                            .put(
                                chunk_key.as_str(),
                                chunk_num.try_into()?,
                                chunking_method,
                                value,
                            )
                            .await?;
                    }
                    (chunk_key, chunk_count)
                }
                ChunkingMethod::InlineBase64 => (
                    base64::encode_config(value.as_bytes().as_ref(), base64::STANDARD_NO_PAD),
                    0,
                ),
            };

            self.data_store
                .put(
                    &key,
                    ctime,
                    chunk_key.as_str(),
                    chunk_count,
                    chunking_method,
                )
                .await
                .map(|()| OverwriteStatus::NotChecked)
        };

        match put_behaviour {
            PutBehaviour::Overwrite => put_fut.await,
            PutBehaviour::IfAbsent | PutBehaviour::OverwriteAndLog => {
                match self.data_store.get(&key).await? {
                    None => {
                        put_fut.await?;
                        Ok(OverwriteStatus::New)
                    }
                    Some(chunked) => {
                        if put_behaviour.should_overwrite() {
                            put_fut.await?;
                            Ok(OverwriteStatus::Overwrote)
                        } else {
                            let chunk_count = chunked.count;
                            for chunk_num in 0..chunk_count {
                                self.chunk_store
                                    .update_generation(
                                        &chunked.id,
                                        chunk_num,
                                        chunked.chunking_method,
                                    )
                                    .await?;
                            }
                            Ok(OverwriteStatus::Prevented)
                        }
                    }
                }
            }
        }
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.put_explicit(ctx, key, value, self.put_behaviour).await
    }
}

#[async_trait]
impl BlobstoreWithLink for Sqlblob {
    async fn link<'a>(
        &'a self,
        _ctx: &'a CoreContext,
        existing_key: &'a str,
        link_key: String,
    ) -> Result<()> {
        let existing_data =
            self.data_store.get(existing_key).await?.ok_or_else(|| {
                format_err!("Key {} does not exist in the blobstore", existing_key)
            })?;
        self.data_store
            .put(
                &link_key,
                existing_data.ctime,
                &existing_data.id,
                existing_data.count,
                existing_data.chunking_method,
            )
            .await
    }

    async fn unlink<'a>(&'a self, _ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        if !self.data_store.is_present(key).await? {
            bail!(
                "Sqlblob::unlink: key {} does not exist in the blobstore",
                key
            )
        };
        self.data_store.unlink(&key).await
    }
}

pub fn set_test_generations(
    source: &TestSource,
    put_generation: i64,
    mark_generation: i64,
    delete_generation: i64,
    mod_time: u64,
) {
    source.insert_config(
        GC_GENERATION_PATH,
        &serde_json::to_string(&XdbGc {
            put_generation,
            mark_generation,
            delete_generation,
        })
        .expect("Invalid input config somehow"),
        ModificationTime::UnixTimestamp(mod_time),
    );
    source.insert_to_refresh(GC_GENERATION_PATH.to_string());
}

pub fn get_test_config_store() -> (Arc<TestSource>, ConfigStore) {
    let test_source = Arc::new(TestSource::new());
    set_test_generations(test_source.as_ref(), 2, 1, 0, INITIAL_VERSION);
    (
        test_source.clone(),
        ConfigStore::new(test_source, UPDATE_FREQUENCY, None),
    )
}
