/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{anyhow, Error};
use assert_matches::assert_matches;
use blobrepo::{save_bonsai_changesets, BlobRepo};
use blobrepo_hg::BlobRepoHg;
use blobstore::Loadable;
use bookmark_renaming::BookmarkRenamer;
use bookmarks::{BookmarkName, BookmarkUpdateReason, Freshness};
use cloned::cloned;
use commit_transformation::upload_commits;
use context::CoreContext;
use cross_repo_sync::types::{Source, Target};
use cross_repo_sync::{rewrite_commit, CommitSyncOutcome, CommitSyncer};
use cross_repo_sync::{
    CandidateSelectionHint, CommitSyncContext, CommitSyncDataProvider, CommitSyncRepos, SyncData,
    CHANGE_XREPO_MAPPING_EXTRA,
};
use fbinit::FacebookInit;
use fixtures::linear;
use futures::{
    compat::{Future01CompatExt, Stream01CompatExt},
    FutureExt, TryFutureExt, TryStreamExt,
};
use futures_ext::FbTryFutureExt;
use manifest::{Entry, ManifestOps};
use maplit::{btreemap, hashmap};
use mercurial_types::HgChangesetId;
use metaconfig_types::CommitSyncConfigVersion;
use mononoke_types::RepositoryId;
use mononoke_types::{ChangesetId, MPath};
use movers::Mover;
use mutable_counters::{MutableCounters, SqlMutableCounters};
use revset::DifferenceOfUnionsOfAncestorsNodeStream;
use skiplist::SkiplistIndex;
use sql_construct::SqlConstruct;
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use synced_commit_mapping::{
    EquivalentWorkingCopyEntry, SqlSyncedCommitMapping, SyncedCommitMapping,
    SyncedCommitMappingEntry, SyncedCommitSourceRepo,
};
use test_repo_factory::TestRepoFactory;
use tests_utils::{
    bookmark, create_commit, list_working_copy_utf8, store_files, store_rename, CreateCommitContext,
};
use tokio::runtime::Runtime;
use tunables::with_tunables_async;

use pretty_assertions::assert_eq;

use crate::{backsync_latest, format_counter, sync_entries, BacksyncLimit, TargetRepoDbs};

const REPOMERGE_FOLDER: &str = "repomerge";
const REPOMERGE_FILE: &str = "repomergefile";
const BRANCHMERGE_FILE: &str = "branchmerge";

#[fbinit::test]
fn backsync_linear(fb: FacebookInit) -> Result<(), Error> {
    let runtime = Runtime::new()?;
    runtime.block_on(async move {
        let (commit_syncer, target_repo_dbs) =
            init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
        backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
    })
}

#[fbinit::test]
fn test_sync_entries(fb: FacebookInit) -> Result<(), Error> {
    // Test makes sure sync_entries() actually sync ALL entries even if transaction
    // for updating bookmark and/or counter failed. This transaction failure is benign and
    // expected, it means that two backsyncers doing the same job in parallel

    let runtime = Runtime::new()?;
    runtime.block_on(async move {
        let (commit_syncer, target_repo_dbs) =
            init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;

        // Backsync a few entries
        let ctx = CoreContext::test_mock(fb);
        backsync_latest(
            ctx.clone(),
            commit_syncer.clone(),
            target_repo_dbs.clone(),
            BacksyncLimit::Limit(2),
        )
        .map_err(Error::from)
        .await?;

        let source_repo = commit_syncer.get_source_repo();
        let target_repo = commit_syncer.get_target_repo();

        let next_log_entries: Vec<_> = source_repo
            .read_next_bookmark_log_entries(ctx.clone(), 0, 1000, Freshness::MostRecent)
            .try_collect()
            .await?;

        // Sync entries starting from counter 0. sync_entries() function should skip
        // 2 first entries, and sync all entries after that
        sync_entries(
            ctx.clone(),
            &commit_syncer,
            target_repo_dbs.clone(),
            next_log_entries.clone(),
            0,
        )
        .await?;

        let latest_log_id = next_log_entries.len() as i64;

        // Make sure all of the entries were synced
        let fetched_value = target_repo_dbs
            .counters
            .get_counter(
                ctx.clone(),
                target_repo.get_repoid(),
                &format_counter(&source_repo.get_repoid()),
            )
            .compat()
            .await?;

        assert_eq!(fetched_value, Some(latest_log_id));

        Ok(())
    })
}

#[fbinit::test]
async fn backsync_linear_with_prefix_mover(fb: FacebookInit) -> Result<(), Error> {
    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Prefix("prefix".to_string()),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_linear_with_mover_that_removes_some_files(fb: FacebookInit) -> Result<(), Error> {
    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Only("files".to_string()),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_linear_with_mover_that_removes_single_file(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Except("10".to_string()),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_linear_bookmark_renamer_only_master(fb: FacebookInit) -> Result<(), Error> {
    let master = BookmarkName::new("master")?;
    let (commit_syncer, target_repo_dbs) =
        init_repos(fb, MoverType::Noop, BookmarkRenamerType::Only(master)).await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_linear_bookmark_renamer_prefix(fb: FacebookInit) -> Result<(), Error> {
    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Noop,
        BookmarkRenamerType::Prefix("prefix".to_string()),
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_linear_bookmark_renamer_remove_all(fb: FacebookInit) -> Result<(), Error> {
    let (commit_syncer, target_repo_dbs) =
        init_repos(fb, MoverType::Noop, BookmarkRenamerType::RemoveAll).await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_linear_bookmark_renamer_and_mover(fb: FacebookInit) -> Result<(), Error> {
    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Except("10".to_string()),
        BookmarkRenamerType::Prefix("prefix".to_string()),
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_two_small_repos(fb: FacebookInit) -> Result<(), Error> {
    let (small_repos, _large_repo, _latest_log_id, dont_verify_commits) =
        init_merged_repos(fb, 2).await?;

    let ctx = CoreContext::test_mock(fb);

    for (commit_syncer, target_repo_dbs) in small_repos {
        let small_repo_id = commit_syncer.get_target_repo().get_repoid();
        println!("backsyncing small repo#{}", small_repo_id.id());

        let small_repo_id = commit_syncer.get_target_repo().get_repoid();
        backsync_latest(
            ctx.clone(),
            commit_syncer.clone(),
            target_repo_dbs.clone(),
            BacksyncLimit::NoLimit,
        )
        .map_err(Error::from)
        .await?;

        println!("verifying small repo#{}", small_repo_id.id());
        verify_mapping_and_all_wc(
            ctx.clone(),
            commit_syncer.clone(),
            dont_verify_commits.clone(),
        )
        .await?;
    }

    Ok(())
}

#[fbinit::test]
async fn backsync_merge_new_repo_all_files_removed(fb: FacebookInit) -> Result<(), Error> {
    let no_newrepo_mover = Arc::new(|path: &MPath| {
        let prefix = MPath::new(REPOMERGE_FOLDER)?;
        let merge_commit_file = MPath::new(REPOMERGE_FILE)?;
        if prefix.is_prefix_of(path) || path == &merge_commit_file {
            Ok(None)
        } else {
            Ok(Some(path.clone()))
        }
    });

    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Custom {
            mover: no_newrepo_mover.clone(),
            // reverse mover is identical to forward mover
            reverse_mover: no_newrepo_mover.clone(),
        },
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_merge_new_repo_branch_removed(fb: FacebookInit) -> Result<(), Error> {
    // Remove all files from new repo except for the file in the merge commit itself
    let no_newrepo_mover = Arc::new(|path: &MPath| {
        let prefix = MPath::new(REPOMERGE_FOLDER)?;
        if prefix.is_prefix_of(path) {
            Ok(None)
        } else {
            Ok(Some(path.clone()))
        }
    });

    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Custom {
            mover: no_newrepo_mover.clone(),
            // reverse mover is identical to forward mover
            reverse_mover: no_newrepo_mover.clone(),
        },
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_branch_merge_remove_branch_merge_file(fb: FacebookInit) -> Result<(), Error> {
    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Except(BRANCHMERGE_FILE.to_string()),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_syncer, target_repo_dbs).await
}

#[fbinit::test]
async fn backsync_unrelated_branch(fb: FacebookInit) -> Result<(), Error> {
    let master = BookmarkName::new("master")?;
    let (commit_syncer, target_repo_dbs) = init_repos(
        fb,
        MoverType::Except("unrelated_branch".to_string()),
        BookmarkRenamerType::Only(master),
    )
    .await?;

    let source_repo = commit_syncer.get_source_repo();

    let ctx = CoreContext::test_mock(fb);
    let merge = build_unrelated_branch(ctx.clone(), &source_repo).await;

    move_bookmark(
        ctx.clone(),
        source_repo.clone(),
        &BookmarkName::new("otherrepo/somebook")?,
        merge,
    )
    .await?;

    backsync_latest(
        ctx.clone(),
        commit_syncer.clone(),
        target_repo_dbs.clone(),
        BacksyncLimit::NoLimit,
    )
    .await?;

    // Unrelated branch should be ignored until it's merged into already backsynced
    // branch
    let maybe_outcome = commit_syncer.get_commit_sync_outcome(&ctx, merge).await?;
    assert!(maybe_outcome.is_none());

    println!("merging into master");
    let new_master =
        CreateCommitContext::new(&ctx, &source_repo, vec!["master", "otherrepo/somebook"])
            .commit()
            .await?;

    move_bookmark(
        ctx.clone(),
        source_repo.clone(),
        &BookmarkName::new("master")?,
        new_master,
    )
    .await?;

    backsync_latest(
        ctx.clone(),
        commit_syncer.clone(),
        target_repo_dbs.clone(),
        BacksyncLimit::NoLimit,
    )
    .await?;
    let maybe_outcome = commit_syncer
        .get_commit_sync_outcome(&ctx, new_master)
        .await?;
    assert!(maybe_outcome.is_some());
    let maybe_outcome = commit_syncer.get_commit_sync_outcome(&ctx, merge).await?;
    assert!(maybe_outcome.is_some());

    Ok(())
}

#[fbinit::test]
async fn backsync_change_mapping(fb: FacebookInit) -> Result<(), Error> {
    // Initialize source and target repos
    let ctx = CoreContext::test_mock(fb);
    let mut factory = TestRepoFactory::new()?;
    let source_repo_id = RepositoryId::new(1);
    let source_repo: BlobRepo = factory.with_id(source_repo_id).build()?;
    let target_repo_id = RepositoryId::new(2);
    let target_repo: BlobRepo = factory.with_id(target_repo_id).build()?;

    // Create commit syncer with two version - current and new
    let target_repo_dbs = TargetRepoDbs {
        connections: factory.metadata_db().clone().into(),
        bookmarks: target_repo.bookmarks().clone(),
        bookmark_update_log: target_repo.bookmark_update_log().clone(),
        counters: SqlMutableCounters::from_sql_connections(factory.metadata_db().clone().into()),
    };
    init_target_repo(&ctx, &target_repo_dbs, source_repo_id, target_repo_id).await?;

    let mapping = SqlSyncedCommitMapping::with_sqlite_in_memory()?;

    let repos = CommitSyncRepos::LargeToSmall {
        large_repo: source_repo.clone(),
        small_repo: target_repo.clone(),
    };

    let current_version = CommitSyncConfigVersion("current_version".to_string());
    let current_mover_type = MoverType::Prefix("current_prefix".to_string());

    let new_version = CommitSyncConfigVersion("new_version".to_string());
    let new_mover_type = MoverType::Prefix("new_prefix".to_string());

    let bookmark_renamer_type = BookmarkRenamerType::Noop;
    let commit_sync_data_provider = CommitSyncDataProvider::test_new(
        current_version.clone(),
        Source(source_repo.get_repoid()),
        Target(target_repo.get_repoid()),
        hashmap! {
            current_version.clone() => SyncData {
                mover: current_mover_type.get_mover(),
                reverse_mover: current_mover_type.get_reverse_mover(),
            },
            new_version.clone() => SyncData{
                mover: new_mover_type.get_mover(),
                reverse_mover: new_mover_type.get_reverse_mover(),
            }
        },
        vec![BookmarkName::new("master")?],
        bookmark_renamer_type.get_bookmark_renamer(),
        bookmark_renamer_type.get_reverse_bookmark_renamer(),
    );
    let commit_syncer =
        CommitSyncer::new_with_provider(&ctx, mapping.clone(), repos, commit_sync_data_provider);

    // Rewrite root commit with current version
    let root_cs_id = CreateCommitContext::new_root(&ctx, &source_repo)
        .commit()
        .await?;

    commit_syncer
        .unsafe_always_rewrite_sync_commit(
            &ctx,
            root_cs_id,
            None,
            &current_version,
            CommitSyncContext::Tests,
        )
        .await?;

    // Add one more empty commit with old mapping
    let before_mapping_change = CreateCommitContext::new(&ctx, &source_repo, vec![root_cs_id])
        .commit()
        .await?;

    // Now create a commit with a special extra that changes the mapping
    // to new version while backsyncing
    let change_mapping_commit =
        CreateCommitContext::new(&ctx, &source_repo, vec![before_mapping_change])
            .add_extra(
                CHANGE_XREPO_MAPPING_EXTRA.to_string(),
                new_version.clone().0.into_bytes(),
            )
            .commit()
            .await?;

    let after_mapping_change_commit =
        CreateCommitContext::new(&ctx, &source_repo, vec![change_mapping_commit])
            .add_file("file", "content")
            .commit()
            .await?;

    bookmark(&ctx, &source_repo, "head")
        .set_to(after_mapping_change_commit)
        .await?;

    // Do the backsync, and check the version
    let tunables = tunables::MononokeTunables::default();
    tunables.update_bools(&hashmap! {
        "allow_change_xrepo_mapping_extra".to_string() => true,
    });

    let f = backsync_latest(
        ctx.clone(),
        commit_syncer.clone(),
        target_repo_dbs.clone(),
        BacksyncLimit::NoLimit,
    );
    with_tunables_async(tunables, f.boxed()).await?;


    let commit_sync_outcome = commit_syncer
        .get_commit_sync_outcome(&ctx, before_mapping_change)
        .await?
        .ok_or_else(|| anyhow!("unexpected missing commit sync outcome"))?;

    assert_matches!(commit_sync_outcome, CommitSyncOutcome::RewrittenAs(_, version) => {
        assert_eq!(current_version, version);
    });

    let commit_sync_outcome = commit_syncer
        .get_commit_sync_outcome(&ctx, change_mapping_commit)
        .await?
        .ok_or_else(|| anyhow!("unexpected missing commit sync outcome"))?;

    assert_matches!(commit_sync_outcome, CommitSyncOutcome::RewrittenAs(_, version) => {
        assert_eq!(new_version, version);
    });

    let commit_sync_outcome = commit_syncer
        .get_commit_sync_outcome(&ctx, after_mapping_change_commit)
        .await?
        .ok_or_else(|| anyhow!("unexpected missing commit sync outcome"))?;

    let target_cs_id = assert_matches!(commit_sync_outcome, CommitSyncOutcome::RewrittenAs(target_cs_id, version) => {
        assert_eq!(new_version, version);
        target_cs_id
    });

    let map = list_working_copy_utf8(&ctx, commit_syncer.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map,
        hashmap! {MPath::new("new_prefix/file")? => "content".to_string()}
    );

    Ok(())
}

async fn build_unrelated_branch(ctx: CoreContext, source_repo: &BlobRepo) -> ChangesetId {
    let p1 = new_commit(
        ctx.clone(),
        source_repo,
        vec![],
        btreemap! {"unrelated_branch" => Some("first content")},
    )
    .await;
    println!("p1: {:?}", p1);

    let p2 = new_commit(
        ctx.clone(),
        source_repo,
        vec![],
        btreemap! {"unrelated_branch" => Some("second content")},
    )
    .await;
    println!("p2: {:?}", p2);

    let merge = new_commit(
        ctx.clone(),
        source_repo,
        vec![p1, p2],
        btreemap! {"unrelated_branch" => Some("merge content")},
    )
    .await;
    println!("merge: {:?}", merge);

    merge
}

async fn new_commit<T: AsRef<str>>(
    ctx: CoreContext,
    repo: &BlobRepo,
    parents: Vec<ChangesetId>,
    contents: BTreeMap<&str, Option<T>>,
) -> ChangesetId {
    create_commit(
        ctx.clone(),
        repo.clone(),
        parents,
        store_files(&ctx, contents, &repo).await,
    )
    .await
}

fn noop_book_renamer(bookmark_name: &BookmarkName) -> Option<BookmarkName> {
    Some(bookmark_name.clone())
}

async fn backsync_and_verify_master_wc(
    fb: FacebookInit,
    commit_syncer: CommitSyncer<SqlSyncedCommitMapping>,
    target_repo_dbs: TargetRepoDbs,
) -> Result<(), Error> {
    let source_repo = commit_syncer.get_source_repo();
    let target_repo = commit_syncer.get_target_repo();

    let ctx = CoreContext::test_mock(fb);
    let next_log_entries: Vec<_> = commit_syncer
        .get_source_repo()
        .read_next_bookmark_log_entries(ctx.clone(), 0, 1000, Freshness::MaybeStale)
        .try_collect()
        .await?;

    let latest_log_id = next_log_entries.len() as i64;

    let mut futs = vec![];
    // Run syncs in parallel
    for _ in 1..5 {
        let f = tokio::task::spawn(backsync_latest(
            ctx.clone(),
            commit_syncer.clone(),
            target_repo_dbs.clone(),
            BacksyncLimit::NoLimit,
        ))
        .flatten_err();
        futs.push(f);
    }

    futures::future::try_join_all(futs).await?;

    // Check that counter was moved
    let fetched_value = target_repo_dbs
        .counters
        .get_counter(
            ctx.clone(),
            target_repo.get_repoid(),
            &format_counter(&source_repo.get_repoid()),
        )
        .compat()
        .await?;
    assert_eq!(fetched_value, Some(latest_log_id));

    verify_mapping_and_all_wc(ctx.clone(), commit_syncer, vec![]).await?;
    Ok(())
}

async fn verify_mapping_and_all_wc(
    ctx: CoreContext,
    commit_syncer: CommitSyncer<SqlSyncedCommitMapping>,
    dont_verify_commits: Vec<ChangesetId>,
) -> Result<(), Error> {
    let source_repo = commit_syncer.get_source_repo();
    let target_repo = commit_syncer.get_target_repo();

    verify_bookmarks(ctx.clone(), commit_syncer.clone()).await?;

    let heads: Vec<_> = source_repo
        .get_bonsai_heads_maybe_stale(ctx.clone())
        .try_collect()
        .await?;

    println!("checking all source commits");
    let all_source_commits = DifferenceOfUnionsOfAncestorsNodeStream::new_union(
        ctx.clone(),
        &source_repo.get_changeset_fetcher(),
        Arc::new(SkiplistIndex::new()),
        heads,
    )
    .compat()
    .try_collect::<Vec<_>>()
    .await?;

    // Check that all commits were synced correctly
    for source_cs_id in all_source_commits {
        if dont_verify_commits.contains(&source_cs_id) {
            continue;
        }
        let csc = commit_syncer.clone();
        let outcome = csc.get_commit_sync_outcome(&ctx, source_cs_id).await?;
        let source_bcs = source_cs_id.load(&ctx, source_repo.blobstore()).await?;
        let outcome = outcome.expect(&format!(
            "commit has not been synced {} {:?}",
            source_cs_id, source_bcs
        ));
        use CommitSyncOutcome::*;

        let (target_cs_id, mover_to_use) = match outcome {
            NotSyncCandidate => {
                continue;
            }
            EquivalentWorkingCopyAncestor(target_cs_id, ref version)
            | RewrittenAs(target_cs_id, ref version) => {
                println!("using mover for {:?}", version);
                (
                    target_cs_id,
                    commit_syncer.get_mover_by_version(version).await?,
                )
            }
        };

        // Empty commits should always be synced, except for merges
        let bcs = source_cs_id
            .load(&ctx, csc.get_source_repo().blobstore())
            .await?;
        if bcs.file_changes().collect::<Vec<_>>().is_empty() && !bcs.is_merge() {
            match outcome {
                RewrittenAs(..) => {}
                _ => {
                    panic!("empty commit should always be remapped {:?}", outcome);
                }
            };
        }

        let source_hg_cs_id = source_repo
            .get_hg_from_bonsai_changeset(ctx.clone(), source_cs_id)
            .await?;
        let target_hg_cs_id = target_repo
            .get_hg_from_bonsai_changeset(ctx.clone(), target_cs_id)
            .await?;

        compare_contents(
            &ctx,
            source_hg_cs_id,
            target_hg_cs_id,
            commit_syncer.clone(),
            mover_to_use.clone(),
        )
        .await?;
    }
    Ok(())
}

async fn verify_bookmarks(
    ctx: CoreContext,
    commit_syncer: CommitSyncer<SqlSyncedCommitMapping>,
) -> Result<(), Error> {
    let source_repo = commit_syncer.get_source_repo();
    let target_repo = commit_syncer.get_target_repo();
    let bookmark_renamer = commit_syncer.get_bookmark_renamer().await?;

    let bookmarks: Vec<_> = source_repo
        .get_publishing_bookmarks_maybe_stale(ctx.clone())
        .try_collect()
        .await?;

    // Check that bookmark point to corresponding working copies
    for (bookmark, source_hg_cs_id) in bookmarks {
        println!("checking bookmark: {}", bookmark.name());
        match bookmark_renamer(&bookmark.name()) {
            Some(renamed_book) => {
                if &renamed_book != bookmark.name() {
                    assert!(
                        target_repo
                            .get_bookmark(ctx.clone(), &bookmark.name())
                            .await?
                            .is_none()
                    );
                }
                let target_hg_cs_id = target_repo
                    .get_bookmark(ctx.clone(), &renamed_book)
                    .await?
                    .expect(&format!(
                        "{} bookmark doesn't exist in target repo!",
                        bookmark.name()
                    ));

                let source_bcs_id = source_repo
                    .get_bonsai_from_hg(ctx.clone(), source_hg_cs_id)
                    .await?
                    .unwrap();

                let commit_sync_outcome = commit_syncer
                    .get_commit_sync_outcome(&ctx, source_bcs_id)
                    .await?;
                let commit_sync_outcome = commit_sync_outcome.expect("unsynced commit");

                println!(
                    "verify_bookmarks. calling compare_contents: source_bcs_id: {}, outcome: {:?}",
                    source_bcs_id, commit_sync_outcome
                );

                use CommitSyncOutcome::*;
                let mover = match commit_sync_outcome {
                    NotSyncCandidate => {
                        panic!("commit should not point to NotSyncCandidate");
                    }
                    EquivalentWorkingCopyAncestor(_, version) | RewrittenAs(_, version) => {
                        println!("using mover for {:?}", version);
                        commit_syncer.get_mover_by_version(&version).await?
                    }
                };

                compare_contents(
                    &ctx,
                    source_hg_cs_id,
                    target_hg_cs_id,
                    commit_syncer.clone(),
                    mover.clone(),
                )
                .await?;
            }
            None => {
                // Make sure we don't have this bookmark in target repo
                assert!(
                    target_repo
                        .get_bookmark(ctx.clone(), &bookmark.name())
                        .await?
                        .is_none()
                );
            }
        }
    }

    Ok(())
}

async fn compare_contents(
    ctx: &CoreContext,
    source_hg_cs_id: HgChangesetId,
    target_hg_cs_id: HgChangesetId,
    commit_syncer: CommitSyncer<SqlSyncedCommitMapping>,
    mover: Mover,
) -> Result<(), Error> {
    let source_content =
        list_content(ctx, source_hg_cs_id, commit_syncer.get_source_repo()).await?;
    let target_content =
        list_content(ctx, target_hg_cs_id, commit_syncer.get_target_repo()).await?;

    println!(
        "source content: {:?}, target content {:?}",
        source_content, target_content
    );
    let filtered_source_content = source_content
        .into_iter()
        .filter_map(|(key, value)| {
            mover(&MPath::new(key).unwrap())
                .unwrap()
                .map(|key| (key, value))
        })
        .map(|(path, value)| (format!("{}", path), value))
        .collect();

    assert_eq!(target_content, filtered_source_content);

    Ok(())
}

async fn list_content(
    ctx: &CoreContext,
    hg_cs_id: HgChangesetId,
    repo: &BlobRepo,
) -> Result<HashMap<String, String>, Error> {
    let cs = hg_cs_id.load(ctx, repo.blobstore()).await?;

    let entries = cs
        .manifestid()
        .list_all_entries(ctx.clone(), repo.get_blobstore())
        .try_collect::<Vec<_>>()
        .await?;

    let mut actual = HashMap::new();
    for (path, entry) in entries {
        match entry {
            Entry::Leaf((_, filenode_id)) => {
                let blobstore = repo.blobstore();
                let envelope = filenode_id.load(ctx, blobstore).await?;
                let content =
                    filestore::fetch_concat(blobstore, ctx, envelope.content_id()).await?;
                let s = String::from_utf8_lossy(content.as_ref()).into_owned();
                actual.insert(format!("{}", path.unwrap()), s);
            }
            Entry::Tree(_) => {}
        }
    }

    Ok(actual)
}

fn identity_mover(v: &MPath) -> Result<Option<MPath>, Error> {
    Ok(Some(v.clone()))
}

enum BookmarkRenamerType {
    Only(BookmarkName),
    RemoveAll,
    Noop,
    Prefix(String),
}

impl BookmarkRenamerType {
    fn get_bookmark_renamer(&self) -> BookmarkRenamer {
        use BookmarkRenamerType::*;

        match self {
            Only(allowed_name) => {
                let allowed_name = allowed_name.clone();
                Arc::new(
                    move |bookmark_name: &BookmarkName| -> Option<BookmarkName> {
                        if bookmark_name == &allowed_name {
                            Some(bookmark_name.clone())
                        } else {
                            None
                        }
                    },
                )
            }
            RemoveAll => Arc::new(|_bookmark_name: &BookmarkName| -> Option<BookmarkName> { None }),
            Noop => Arc::new(noop_book_renamer),
            Prefix(prefix) => {
                let prefix = prefix.clone();
                Arc::new(
                    move |bookmark_name: &BookmarkName| -> Option<BookmarkName> {
                        Some(BookmarkName::new(format!("{}/{}", prefix, bookmark_name)).unwrap())
                    },
                )
            }
        }
    }

    fn get_reverse_bookmark_renamer(&self) -> BookmarkRenamer {
        use BookmarkRenamerType::*;

        match self {
            Only(..) | RemoveAll | Noop => {
                // All these three cases have bookmark_renamer == reverse_bookmark_renamer
                self.get_bookmark_renamer()
            }
            Prefix(prefix) => {
                let prefix = prefix.clone();
                Arc::new(
                    move |bookmark_name: &BookmarkName| -> Option<BookmarkName> {
                        if bookmark_name.as_str().starts_with(prefix.as_str()) {
                            let unprefixed = &bookmark_name.as_ascii()[prefix.len()..];
                            Some(BookmarkName::new_ascii(unprefixed.into()))
                        } else {
                            None
                        }
                    },
                )
            }
        }
    }
}

enum MoverType {
    Noop,
    Except(String),
    Prefix(String),
    Only(String),
    Custom { mover: Mover, reverse_mover: Mover },
}

impl MoverType {
    fn get_mover(&self) -> Mover {
        use MoverType::*;

        match self {
            Noop => Arc::new(identity_mover),
            Prefix(prefix) => {
                let prefix = MPath::new(prefix).unwrap();
                Arc::new(move |path: &MPath| Ok(Some(MPath::join(&prefix, path))))
            }
            Except(file) => {
                let forbidden = MPath::new(file).unwrap();
                Arc::new(move |path: &MPath| {
                    if path == &forbidden {
                        Ok(None)
                    } else {
                        Ok(Some(path.clone()))
                    }
                })
            }
            Only(file) => {
                let allowed = MPath::new(file).unwrap();
                Arc::new(move |path: &MPath| {
                    if path == &allowed {
                        Ok(Some(path.clone()))
                    } else {
                        Ok(None)
                    }
                })
            }
            Custom { mover, .. } => mover.clone(),
        }
    }

    fn get_reverse_mover(&self) -> Mover {
        use MoverType::*;

        match self {
            Noop | Only(..) | Except(..) => self.get_mover(),
            Prefix(prefix) => {
                let prefix = MPath::new(prefix).unwrap();
                Arc::new(move |path: &MPath| Ok(path.remove_prefix_component(&prefix)))
            }
            Custom { reverse_mover, .. } => reverse_mover.clone(),
        }
    }
}

async fn init_repos(
    fb: FacebookInit,
    mover_type: MoverType,
    bookmark_renamer_type: BookmarkRenamerType,
) -> Result<(CommitSyncer<SqlSyncedCommitMapping>, TargetRepoDbs), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mut factory = TestRepoFactory::new()?;
    let source_repo_id = RepositoryId::new(1);
    let source_repo: BlobRepo = factory.with_id(source_repo_id).build()?;
    linear::initrepo(fb, &source_repo).await;

    let target_repo_id = RepositoryId::new(2);
    let target_repo: BlobRepo = factory.with_id(target_repo_id).build()?;

    let target_repo_dbs = TargetRepoDbs {
        connections: factory.metadata_db().clone().into(),
        bookmarks: target_repo.bookmarks().clone(),
        bookmark_update_log: target_repo.bookmark_update_log().clone(),
        counters: SqlMutableCounters::from_sql_connections(factory.metadata_db().clone().into()),
    };
    init_target_repo(&ctx, &target_repo_dbs, source_repo_id, target_repo_id).await?;

    let mapping = SqlSyncedCommitMapping::with_sqlite_in_memory()?;

    let repos = CommitSyncRepos::LargeToSmall {
        large_repo: source_repo.clone(),
        small_repo: target_repo.clone(),
    };

    let empty: BTreeMap<_, Option<&str>> = BTreeMap::new();
    // Create fake empty commit in the target repo
    let initial_commit_in_target = create_commit(
        ctx.clone(),
        target_repo.clone(),
        vec![],
        store_files(&ctx, empty.clone(), &source_repo).await,
    )
    .await;

    let current_version = CommitSyncConfigVersion("TEST_VERSION_NAME".to_string());
    let commit_sync_data_provider = CommitSyncDataProvider::test_new(
        current_version.clone(),
        Source(source_repo.get_repoid()),
        Target(target_repo.get_repoid()),
        hashmap! {
            current_version => SyncData {
                mover: mover_type.get_mover(),
                reverse_mover: mover_type.get_reverse_mover(),
            }
        },
        vec![BookmarkName::new("master")?],
        bookmark_renamer_type.get_bookmark_renamer(),
        bookmark_renamer_type.get_reverse_bookmark_renamer(),
    );
    let commit_syncer =
        CommitSyncer::new_with_provider(&ctx, mapping.clone(), repos, commit_sync_data_provider);

    // Sync first commit manually
    let initial_bcs_id = source_repo
        .get_bonsai_from_hg(
            ctx.clone(),
            HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536").unwrap(),
        )
        .await?
        .unwrap();
    let first_bcs = initial_bcs_id.load(&ctx, source_repo.blobstore()).await?;
    upload_commits(&ctx, vec![first_bcs.clone()], &source_repo, &target_repo).await?;
    let first_bcs_mut = first_bcs.into_mut();
    let maybe_rewritten = {
        let empty_map = HashMap::new();
        cloned!(ctx, source_repo);
        rewrite_commit(
            &ctx,
            first_bcs_mut,
            &empty_map,
            mover_type.get_mover(),
            source_repo,
        )
        .await
    }?;
    let rewritten_first_bcs_id = match maybe_rewritten {
        Some(mut rewritten) => {
            rewritten.parents.push(initial_commit_in_target);

            let rewritten = rewritten.freeze()?;
            save_bonsai_changesets(vec![rewritten.clone()], ctx.clone(), target_repo.clone())
                .await?;
            rewritten.get_changeset_id()
        }
        None => initial_commit_in_target,
    };

    let first_entry = SyncedCommitMappingEntry::new(
        source_repo.get_repoid(),
        initial_bcs_id,
        target_repo.get_repoid(),
        rewritten_first_bcs_id,
        CommitSyncConfigVersion("TEST_VERSION_NAME".to_string()),
        commit_syncer.get_source_repo_type(),
    );
    mapping.add(&ctx, first_entry).await?;

    // Create a few new commits on top of master

    let master = BookmarkName::new("master")?;
    let master_val = source_repo
        .get_bonsai_bookmark(ctx.clone(), &master)
        .await?
        .unwrap();

    let empty_bcs_id = create_commit(
        ctx.clone(),
        source_repo.clone(),
        vec![master_val],
        store_files(&ctx, empty, &source_repo).await,
    )
    .await;

    let first_bcs_id = create_commit(
        ctx.clone(),
        source_repo.clone(),
        vec![empty_bcs_id],
        store_files(
            &ctx,
            btreemap! {"randomfile" => Some("some content")},
            &source_repo,
        )
        .await,
    )
    .await;

    let second_bcs_id = create_commit(
        ctx.clone(),
        source_repo.clone(),
        vec![first_bcs_id],
        store_files(
            &ctx,
            btreemap! {"randomfile" => Some("some other content")},
            &source_repo,
        )
        .await,
    )
    .await;

    move_bookmark(ctx.clone(), source_repo.clone(), &master, second_bcs_id).await?;

    // Create new bookmark
    let master = BookmarkName::new("anotherbookmark")?;
    move_bookmark(ctx.clone(), source_repo.clone(), &master, first_bcs_id).await?;

    // Merge new repo into master
    let first_new_repo_file = format!("{}/first", REPOMERGE_FOLDER);
    let to_remove_new_repo_file = format!("{}/toremove", REPOMERGE_FOLDER);
    let move_dest_new_repo_file = format!("{}/movedest", REPOMERGE_FOLDER);
    let second_new_repo_file = format!("{}/second", REPOMERGE_FOLDER);

    let first_new_repo_commit = new_commit(
        ctx.clone(),
        &source_repo,
        vec![],
        btreemap! {
            first_new_repo_file.as_ref() => Some("new repo content"),
            to_remove_new_repo_file.as_ref() => Some("new repo content"),
        },
    )
    .await;

    let p2 = {
        let (path_rename, rename_file_change) = store_rename(
            &ctx,
            (
                MPath::new(to_remove_new_repo_file.clone())?,
                first_new_repo_commit,
            ),
            &move_dest_new_repo_file,
            "moved content",
            &source_repo,
        )
        .await;

        let mut stored_files = store_files(
            &ctx,
            btreemap! {
                second_new_repo_file.as_ref() => Some("new repo second content"),
            },
            &source_repo,
        )
        .await;
        stored_files.insert(path_rename, rename_file_change);

        create_commit(
            ctx.clone(),
            source_repo.clone(),
            vec![first_new_repo_commit],
            stored_files,
        )
        .await
    };

    let merge = new_commit(
        ctx.clone(),
        &source_repo,
        vec![second_bcs_id, p2],
        btreemap! {
             REPOMERGE_FILE => Some("some content"),
        },
    )
    .await;
    move_bookmark(ctx.clone(), source_repo.clone(), &master, merge).await?;

    // Create a branch merge - merge initial commit in the repo with the last
    let branch_merge_p1 = new_commit(
        ctx.clone(),
        &source_repo,
        vec![initial_bcs_id],
        btreemap! {
            "3" => Some("branchmerge 3 content"),
        },
    )
    .await;

    let branch_merge = new_commit(
        ctx.clone(),
        &source_repo,
        vec![branch_merge_p1, merge],
        btreemap! {
            BRANCHMERGE_FILE => Some("branch merge content"),
            // Both parents have different content in "files" and "3" - need to resolve it
            "files" => Some("branchmerge files content"),
            "3" => Some("merged 3"),
        },
    )
    .await;
    move_bookmark(ctx.clone(), source_repo.clone(), &master, branch_merge).await?;

    // Do a branch merge again, but this time only change content in BRANCHMERGE_FILE
    let branch_merge_second = new_commit(
        ctx.clone(),
        &source_repo,
        vec![branch_merge_p1, branch_merge],
        btreemap! {
            BRANCHMERGE_FILE => Some("new branch merge content"),
            // Both parents have different content in "files" and "3" - need to resolve it
            "files" => Some("branchmerge files content"),
            "3" => Some("merged 3"),
        },
    )
    .await;
    move_bookmark(
        ctx.clone(),
        source_repo.clone(),
        &master,
        branch_merge_second,
    )
    .await?;

    Ok((commit_syncer, target_repo_dbs))
}

async fn init_target_repo(
    ctx: &CoreContext,
    target_repo_dbs: &TargetRepoDbs,
    source_repo_id: RepositoryId,
    target_repo_id: RepositoryId,
) -> Result<(), Error> {
    // Init counters
    target_repo_dbs
        .counters
        .set_counter(
            ctx.clone(),
            target_repo_id,
            &format_counter(&source_repo_id),
            0,
            None,
        )
        .compat()
        .await?;

    Ok(())
}

async fn init_merged_repos(
    fb: FacebookInit,
    num_repos: usize,
) -> Result<
    (
        Vec<(CommitSyncer<SqlSyncedCommitMapping>, TargetRepoDbs)>,
        BlobRepo,
        i64,
        Vec<ChangesetId>,
    ),
    Error,
> {
    let ctx = CoreContext::test_mock(fb);

    let mut factory = TestRepoFactory::new()?;
    let large_repo_id = RepositoryId::new(num_repos as i32);
    let large_repo: BlobRepo = factory.with_id(large_repo_id).build()?;

    let mapping = SqlSyncedCommitMapping::with_sqlite_in_memory()?;

    let mut output = vec![];
    let mut small_repos = vec![];
    let mut moved_cs_ids = vec![];
    // Create small repos and one large repo
    for idx in 0..num_repos {
        let repoid = RepositoryId::new(idx as i32);
        let small_repo: BlobRepo = factory.with_id(repoid).build()?;
        let small_repo_dbs = TargetRepoDbs {
            connections: factory.metadata_db().clone().into(),
            bookmarks: small_repo.bookmarks().clone(),
            bookmark_update_log: small_repo.bookmark_update_log().clone(),
            counters: SqlMutableCounters::from_sql_connections(
                factory.metadata_db().clone().into(),
            ),
        };

        // Init counters
        small_repo_dbs
            .counters
            .set_counter(
                ctx.clone(),
                repoid,
                &format_counter(&large_repo_id),
                0,
                None,
            )
            .compat()
            .await?;
        let bookmark_renamer = Arc::new(
            move |bookmark_name: &BookmarkName| -> Option<BookmarkName> {
                let master = BookmarkName::new("master").unwrap();
                let name = format!("{}", bookmark_name);
                let prefix = format!("smallrepo{}", repoid.id());
                if bookmark_name == &master {
                    Some(master)
                } else if name.starts_with(&prefix) {
                    Some(BookmarkName::new(&name[prefix.len()..]).unwrap())
                } else {
                    None
                }
            },
        );

        let reverse_bookmark_renamer = Arc::new(
            move |bookmark_name: &BookmarkName| -> Option<BookmarkName> {
                let master = BookmarkName::new("master").unwrap();
                let name = format!("{}", bookmark_name);
                let prefix = format!("smallrepo{}", repoid.id());
                if bookmark_name == &master {
                    Some(master)
                } else {
                    Some(BookmarkName::new(format!("{}{}", prefix, name)).unwrap())
                }
            },
        );

        let mover_type = MoverType::Prefix(format!("smallrepo{}", small_repo.get_repoid().id()));
        let repos = CommitSyncRepos::LargeToSmall {
            large_repo: large_repo.clone(),
            small_repo: small_repo.clone(),
        };
        let current_version = CommitSyncConfigVersion("TEST_VERSION_NAME".to_string());
        let noop_version = CommitSyncConfigVersion("noop".to_string());
        let commit_sync_data_provider = CommitSyncDataProvider::test_new(
            current_version.clone(),
            Source(large_repo.get_repoid()),
            Target(small_repo.get_repoid()),
            hashmap! {
                current_version => SyncData {
                    mover: mover_type.get_reverse_mover(),
                    reverse_mover: mover_type.get_mover(),
                },
                noop_version => SyncData {
                    mover: Arc::new(identity_mover),
                    reverse_mover: Arc::new(identity_mover),
                }
            },
            vec![BookmarkName::new("master")?],
            bookmark_renamer.clone(),
            reverse_bookmark_renamer.clone(),
        );

        let commit_syncer = CommitSyncer::new_with_provider(
            &ctx,
            mapping.clone(),
            repos,
            commit_sync_data_provider,
        );
        output.push((commit_syncer, small_repo_dbs));

        let filename = format!("file_in_smallrepo{}", small_repo.get_repoid().id());
        let small_repo_cs_id = create_commit(
            ctx.clone(),
            small_repo.clone(),
            vec![],
            store_files(
                &ctx,
                btreemap! { filename.as_str() => Some("some content")},
                &small_repo,
            )
            .await,
        )
        .await;
        println!("small repo cs id w/o parents: {}", small_repo_cs_id);

        small_repos.push((small_repo.clone(), small_repo_cs_id.clone()));

        let mut other_repo_ids = vec![];
        for i in 0..num_repos {
            if i != idx {
                other_repo_ids.push(RepositoryId::new(i as i32));
            }
        }

        preserve_premerge_commit(
            ctx.clone(),
            large_repo.clone(),
            small_repo.clone(),
            other_repo_ids.clone(),
            small_repo_cs_id,
            &mapping,
        )
        .await?;

        let renamed_filename = format!("smallrepo{}/{}", small_repo.get_repoid().id(), filename);
        let (renamed_path, rename) = store_rename(
            &ctx,
            (MPath::new(&filename).unwrap(), small_repo_cs_id),
            renamed_filename.as_str(),
            "some content",
            &large_repo,
        )
        .await;

        let moved_cs_id = create_commit(
            ctx.clone(),
            large_repo.clone(),
            vec![small_repo_cs_id],
            btreemap! {
                renamed_path => rename,
            },
        )
        .await;
        println!("large repo moved cs id: {}", moved_cs_id);
        moved_cs_ids.push(moved_cs_id);
    }

    // Create merge commit
    let merge_cs_id = create_commit(
        ctx.clone(),
        large_repo.clone(),
        moved_cs_ids.clone(),
        btreemap! {},
    )
    .await;

    println!("large repo merge cs id: {}", merge_cs_id);
    // Create an empty commit on top of a merge commit and sync it to all small repos
    let empty: BTreeMap<_, Option<&str>> = BTreeMap::new();
    // Create empty commit in the large repo, and sync it to all small repos
    let first_after_merge_commit = create_commit(
        ctx.clone(),
        large_repo.clone(),
        vec![merge_cs_id],
        store_files(&ctx, empty.clone(), &large_repo).await,
    )
    .await;
    println!("large repo empty commit: {}", first_after_merge_commit);

    for (small_repo, latest_small_repo_cs_id) in &small_repos {
        let small_repo_first_after_merge = create_commit(
            ctx.clone(),
            small_repo.clone(),
            vec![*latest_small_repo_cs_id],
            store_files(&ctx, empty.clone(), &small_repo).await,
        )
        .await;

        println!(
            "empty commit in {}: {}",
            small_repo.get_repoid(),
            small_repo_first_after_merge
        );
        let entry = SyncedCommitMappingEntry::new(
            large_repo.get_repoid(),
            first_after_merge_commit,
            small_repo.get_repoid(),
            small_repo_first_after_merge,
            CommitSyncConfigVersion("TEST_VERSION_NAME".to_string()),
            SyncedCommitSourceRepo::Large,
        );
        mapping.add(&ctx, entry).await?;
    }

    // Create new commit in large repo
    let mut latest_log_id = 0;
    {
        let master = BookmarkName::new("master")?;
        let mut prev_master = None;
        for repo_id in 0..num_repos {
            let filename = format!("smallrepo{}/newfile", repo_id);
            let new_commit = create_commit(
                ctx.clone(),
                large_repo.clone(),
                vec![first_after_merge_commit],
                store_files(
                    &ctx,
                    btreemap! { filename.as_str() => Some("new content")},
                    &large_repo,
                )
                .await,
            )
            .await;

            println!("new commit in large repo: {}", new_commit);
            latest_log_id += 1;
            move_bookmark(ctx.clone(), large_repo.clone(), &master, new_commit).await?;
            prev_master = Some(new_commit);
        }

        // Create bookmark on premerge commit from first repo
        let premerge_book = BookmarkName::new("smallrepo0/premerge_book")?;
        latest_log_id += 1;
        move_bookmark(
            ctx.clone(),
            large_repo.clone(),
            &premerge_book,
            small_repos[0].1,
        )
        .await?;

        // Now on second repo and move it to rewritten changeset
        let premerge_book = BookmarkName::new("smallrepo1/premerge_book")?;
        latest_log_id += 1;
        move_bookmark(
            ctx.clone(),
            large_repo.clone(),
            &premerge_book,
            small_repos[1].1,
        )
        .await?;

        latest_log_id += 1;
        move_bookmark(
            ctx.clone(),
            large_repo.clone(),
            &premerge_book,
            prev_master.unwrap(),
        )
        .await?;

        // New commit that touches files from two different small repos
        let filename1 = "smallrepo0/newfile";
        let filename2 = "smallrepo1/newfile";
        let new_commit = create_commit(
            ctx.clone(),
            large_repo.clone(),
            vec![first_after_merge_commit],
            store_files(
                &ctx,
                btreemap! {
                    filename1 => Some("new content1"),
                    filename2 => Some("new content2"),
                },
                &large_repo,
            )
            .await,
        )
        .await;
        println!("large_repo newcommit: {}", new_commit);

        latest_log_id += 1;
        move_bookmark(ctx.clone(), large_repo.clone(), &master, new_commit).await?;

        // Create a Preserved commit
        let premerge_book = BookmarkName::new("smallrepo0/preserved_commit")?;
        let filename = "smallrepo1/newfile";
        let new_commit = create_commit(
            ctx.clone(),
            large_repo.clone(),
            vec![small_repos[0].1],
            store_files(
                &ctx,
                btreemap! {
                    filename => Some("preserved content"),
                },
                &large_repo,
            )
            .await,
        )
        .await;
        println!("smallrepo1 newcommit: {}", new_commit);

        latest_log_id += 1;
        move_bookmark(ctx.clone(), large_repo.clone(), &premerge_book, new_commit).await?;
    }

    let mut commits_to_skip_verification = vec![];
    commits_to_skip_verification.extend(moved_cs_ids);
    commits_to_skip_verification.push(merge_cs_id);

    Ok((
        output,
        large_repo,
        latest_log_id,
        commits_to_skip_verification,
    ))
}

async fn preserve_premerge_commit(
    ctx: CoreContext,
    large_repo: BlobRepo,
    small_repo: BlobRepo,
    another_small_repo_ids: Vec<RepositoryId>,
    bcs_id: ChangesetId,
    mapping: &SqlSyncedCommitMapping,
) -> Result<(), Error> {
    println!(
        "preserve_premerge_commit called. large_repo: {}; small_repo: {}, another_small_repo_ids: {:?}, bcs_id: {}",
        large_repo.get_repoid(),
        small_repo.get_repoid(),
        another_small_repo_ids,
        bcs_id
    );

    // Doesn't matter what mover to use - we are going to preserve the commit anyway
    let bookmark_renamer = Arc::new(noop_book_renamer);
    let small_to_large_sync_config = {
        let repos = CommitSyncRepos::SmallToLarge {
            large_repo: large_repo.clone(),
            small_repo: small_repo.clone(),
        };

        let noop_version = CommitSyncConfigVersion("noop".to_string());
        let commit_sync_data_provider = CommitSyncDataProvider::test_new(
            noop_version.clone(),
            Source(small_repo.get_repoid()),
            Target(large_repo.get_repoid()),
            hashmap! {
                noop_version => SyncData {
                    mover: Arc::new(identity_mover),
                    reverse_mover: Arc::new(identity_mover),
                }
            },
            vec![BookmarkName::new("master")?],
            bookmark_renamer.clone(),
            bookmark_renamer.clone(),
        );
        CommitSyncer::new_with_provider(&ctx, mapping.clone(), repos, commit_sync_data_provider)
    };

    small_to_large_sync_config
        .unsafe_sync_commit_with_expected_version(
            &ctx,
            bcs_id,
            CandidateSelectionHint::Only,
            CommitSyncConfigVersion("noop".to_string()),
            CommitSyncContext::Tests,
        )
        .await?;

    for another_repo_id in another_small_repo_ids {
        mapping
            .insert_equivalent_working_copy(
                &ctx,
                EquivalentWorkingCopyEntry {
                    large_repo_id: large_repo.get_repoid(),
                    large_bcs_id: bcs_id,
                    small_repo_id: another_repo_id,
                    small_bcs_id: None,
                    version_name: None,
                },
            )
            .await?;
    }
    Ok(())
}

async fn move_bookmark(
    ctx: CoreContext,
    repo: BlobRepo,
    bookmark: &BookmarkName,
    bcs_id: ChangesetId,
) -> Result<(), Error> {
    let mut txn = repo.update_bookmark_transaction(ctx.clone());

    let prev_bcs_id = repo.get_bonsai_bookmark(ctx, bookmark).await?;

    match prev_bcs_id {
        Some(prev_bcs_id) => {
            txn.update(
                bookmark,
                bcs_id,
                prev_bcs_id,
                BookmarkUpdateReason::TestMove,
                None,
            )?;
        }
        None => {
            txn.create(bookmark, bcs_id, BookmarkUpdateReason::TestMove, None)?;
        }
    }

    assert!(txn.commit().await?);
    Ok(())
}
