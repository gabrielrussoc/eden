/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::cell::RefCell;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread_local;
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use cached_config::ConfigHandle;
use futures::{future::poll_fn, Future, FutureExt};
use once_cell::sync::OnceCell;
use slog::{debug, warn, Logger};
use std::sync::atomic::{AtomicBool, AtomicI64};

use tunables_derive::Tunables;
use tunables_structs::Tunables as TunablesStruct;

use std::collections::HashMap;

static TUNABLES: OnceCell<MononokeTunables> = OnceCell::new();
static TUNABLES_WORKER_STATE: OnceCell<Mutex<TunablesWorkerState>> = OnceCell::new();
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

thread_local! {
    static TUNABLES_OVERRIDE: RefCell<Option<Arc<MononokeTunables>>> = RefCell::new(None);
}

pub enum TunablesReference {
    Override(Arc<MononokeTunables>),
    Static(&'static MononokeTunables),
}

impl Deref for TunablesReference {
    type Target = MononokeTunables;

    fn deref(&self) -> &MononokeTunables {
        match self {
            Self::Override(r) => r.as_ref(),
            Self::Static(r) => r,
        }
    }
}

pub fn tunables() -> TunablesReference {
    TUNABLES_OVERRIDE.with(|tunables_override| match *tunables_override.borrow() {
        Some(ref arc) => TunablesReference::Override(arc.clone()),
        None => TunablesReference::Static(TUNABLES.get_or_init(MononokeTunables::default)),
    })
}

// This type exists to simplify code generation in tunables-derive
pub type TunableString = ArcSwap<String>;

pub type TunableBoolByRepo = ArcSwap<HashMap<String, bool>>;
pub type TunableStringByRepo = ArcSwap<HashMap<String, String>>;
pub type TunableVecOfStringsByRepo = ArcSwap<HashMap<String, Vec<String>>>;
pub type TunableI64ByRepo = ArcSwap<HashMap<String, i64>>;

#[derive(Tunables, Default, Debug)]
pub struct MononokeTunables {
    mutation_advertise_for_infinitepush: AtomicBool,
    mutation_accept_for_infinitepush: AtomicBool,
    mutation_generate_for_draft: AtomicBool,
    warm_bookmark_cache_delay: AtomicI64,
    warm_bookmark_cache_poll_interval_ms: AtomicI64,
    /// Don't read from the BookmarksSubscription when updating the WBC, and instead poll for the
    /// entire list of bookmarks on every iteration.
    warm_bookmark_cache_disable_subscription: AtomicBool,
    /// Maximum age of bookmarks subscriptions.
    bookmark_subscription_max_age_ms: AtomicI64,
    bookmark_subscription_protect_master: AtomicBool,
    max_scuba_msg_length: AtomicI64,
    wishlist_read_qps: AtomicI64,
    wishlist_write_qps: AtomicI64,
    command_monitor_interval: AtomicI64,
    command_monitor_remote_logging: AtomicI64,
    // Log all getfiles/gettreepack requests for paths that start with prefix
    // in a particular repo
    undesired_path_repo_name_to_log: TunableString,
    undesired_path_prefix_to_log: TunableString,
    undesired_path_regex_to_log: TunableString,
    pushrebase_disable_rebased_commit_validation: AtomicBool,
    filenodes_disabled: AtomicBool,
    filenodes_master_fallback_ratio: AtomicI64,
    // Skiplist config
    skiplist_max_skips_without_yield: AtomicI64,
    skiplist_reload_disabled: AtomicBool,
    skiplist_reload_interval: AtomicI64,
    deduplicated_put_sampling_rate: AtomicI64,
    disable_repo_client_warm_bookmarks_cache: AtomicBool,
    remotefilelog_file_history_limit: AtomicI64,
    disable_hooks_on_plain_push: AtomicBool,
    run_hooks_on_additional_changesets: AtomicBool,
    hooks_additional_changesets_limit: AtomicI64,
    // SCS scuba sampling knobs
    scs_popular_methods_sampling_rate: AtomicI64,
    scs_other_methods_sampling_rate: AtomicI64,
    // When false error logs are never sampled
    scs_error_log_sampling: AtomicBool,
    redacted_logging_sampling_rate: AtomicI64,
    redaction_config_from_xdb: AtomicBool,
    getbundle_use_low_gen_optimization: AtomicBool,
    getbundle_high_low_gen_num_difference_threshold: AtomicI64,
    getbundle_low_gen_optimization_max_traversal_limit: AtomicI64,
    getbundle_partial_getbundle_traversal_limit: AtomicI64,
    repo_client_bookmarks_timeout_secs: AtomicI64,
    repo_client_clone_timeout_secs: AtomicI64,
    repo_client_default_timeout_secs: AtomicI64,
    repo_client_getbundle_timeout_secs: AtomicI64,
    repo_client_getpack_timeout_secs: AtomicI64,
    repo_client_concurrent_blob_uploads: AtomicI64,
    repo_client_max_nodes_in_known_method: AtomicI64,
    // How many trees is getting prepared at once
    repo_client_gettreepack_buffer_size: AtomicI64,
    derived_data_slow_derivation_threshold_secs: AtomicI64,
    disable_running_hooks_in_pushredirected_repo: AtomicBool,
    scs_request_read_qps: AtomicI64,
    scs_request_write_qps: AtomicI64,
    enable_logging_commit_rewrite_data: AtomicBool,
    // All blobstore read request with size bigger than
    // this threshold will be logged to scuba
    blobstore_read_size_logging_threshold: AtomicI64,
    hash_validation_percentage: AtomicI64,
    // Filter out commits that we already have in infinitepush. Shouldn't be needed if we have a
    // client exchanging commits with us, but when processing bundled uploads (i.e. commit cloud
    // filling), it might help a lot.
    filter_pre_existing_commits_on_infinitepush: AtomicBool,
    backfill_read_qps: AtomicI64,
    backfill_write_qps: AtomicI64,
    disable_commit_scribe_logging_scs: AtomicBool,
    xrepo_sync_disable_all_syncs: AtomicBool,
    xrepo_disable_commit_sync_lease: AtomicBool,

    // Use Background session class while deriving data. This makes derived data not write
    // data to blobstore sync queue if a write was successful to the main blobstore.
    derived_data_use_background_session_class: TunableBoolByRepo,
    commit_cloud_use_background_session_class: AtomicBool,
    multiplex_blobstore_background_session_timeout_ms: AtomicI64,

    allow_change_xrepo_mapping_extra: AtomicBool,

    // Disable EdenAPI in http_service.
    disable_http_service_edenapi: AtomicBool,

    // Disable putting hydrating manifests in .hg
    disable_hydrating_manifests_in_dot_hg: AtomicBool,

    // Rendez vous configuration.
    rendezvous_dispatch_delay_ms: AtomicI64,
    rendezvous_dispatch_max_threshold: AtomicI64,

    unbundle_limit_num_of_commits_in_push: AtomicI64,

    // Maximium negative caching age of a blob, in milliseconds
    // Negative means to not use weak consistency at all
    manifold_weak_consistency_max_age_ms: AtomicI64,

    // -1: No override, use manifold server side config and rollout checks
    // if set to > -1, set the client side option to override (see manifoldblob code)
    manifold_request_priority_override: AtomicI64,

    // Frequency at which to collect SQL connection pool stats
    sql_connection_pool_stats_collection_interval_ms: AtomicI64,

    bookmarks_cache_ttl_ms: AtomicI64,

    // Disable running SaveMappingPushrebaseHook on every Pushrebase
    disable_save_mapping_pushrebase_hook: AtomicBool,

    // Set to 0 to disable compression
    zstd_compression_level: AtomicI64,

    // Commits that aren't related (i.e. that are not ancestors of each other)
    // can be derived in parallel, and that's what derived data does.
    // derived_data_disable_parallel_derivation is a killswitch to disable
    // parallel derivation, derived_data_parallel_derivation_buffer limits
    // how many commits will be derived at once.
    derived_data_disable_parallel_derivation: AtomicBool,
    derived_data_parallel_derivation_buffer: AtomicI64,

    // Tunables to disable derived data derivation either for the full repo
    // or for specific derived data types inside a repo
    all_derived_data_disabled: TunableBoolByRepo,
    derived_data_types_disabled: TunableVecOfStringsByRepo,
    // How often to check if derived data is disabled or not
    derived_data_disabled_watcher_delay_secs: AtomicI64,

    // Disable derivation via direved data service.
    derived_data_disable_remote_derivation: AtomicBool,

    // multiplexed blobstore is_present/get new semantics rollout
    multiplex_blobstore_get_do_queue_lookup: AtomicBool,
    multiplex_blobstore_is_present_do_queue_lookup: AtomicBool,

    fastlog_use_mutable_renames: TunableBoolByRepo,
    megarepo_api_dont_set_file_mutable_renames: AtomicBool,
    megarepo_api_dont_set_directory_mutable_renames: AtomicBool,

    force_unode_v2: AtomicBool,
    fastlog_use_gen_num_traversal: AtomicBool,
    fsnodes_use_new_batch_derivation: AtomicBool,
    unodes_use_new_batch_derivation: AtomicBool,
    skeleton_manifests_use_new_batch_derivation: AtomicBool,

    // Changing the value of this tunable forces all mononoke instances
    // to reload segmented changelog. One can also specify jitter (or use default)
    segmented_changelog_force_reload: TunableI64ByRepo,
    segmented_changelog_force_reload_jitter_secs: AtomicI64,

    // This tunable gates the usage of segmented changelog for speeding up
    // server-side operations (like for example SCS ancestry checks)
    segmented_changelog_disable_for_server_side_ops: AtomicBool,

    // Use comprehensive mode for is_present method in multiplexed blobstore for edenapi lookup api.
    edenapi_lookup_use_comprehensive_mode: AtomicBool,

    // Timeout for is_present call for multiplexed blobstore
    is_present_timeout_ms: AtomicI64,
}

fn log_tunables(tunables: &TunablesStruct) -> String {
    serde_json::to_string(tunables)
        .unwrap_or_else(|e| format!("failed to serialize tunables: {}", e))
}

pub fn init_tunables_worker(
    logger: Logger,
    config_handle: ConfigHandle<TunablesStruct>,
) -> Result<()> {
    let init_tunables = config_handle.get();
    debug!(
        logger,
        "Initializing tunables: {}",
        log_tunables(&init_tunables)
    );
    update_tunables(init_tunables.clone())?;

    if TUNABLES_WORKER_STATE
        .set(Mutex::new(TunablesWorkerState {
            config_handle,
            old_tunables: Some(init_tunables),
            logger,
        }))
        .is_err()
    {
        panic!("Two or more tunables update threads exist at the same time");
    }

    thread::Builder::new()
        .name("mononoke-tunables".into())
        .spawn(worker)
        .expect("Can't spawn tunables updater");

    Ok(())
}

/// Tunables are updated in loop with sleeps. Call this to force update them.
/// Meant to be used in tests.
/// NOTE: if tunables are fetched from Configerator, you need to force update it as well.
pub fn force_update_tunables() {
    worker_iteration();
}

struct TunablesWorkerState {
    config_handle: ConfigHandle<TunablesStruct>,
    // Previous value of the tunables.  If we fail to update tunables,
    // this will be `None`.
    old_tunables: Option<Arc<TunablesStruct>>,
    logger: Logger,
}

fn worker() {
    loop {
        // TODO: Instead of refreshing tunables every loop iteration,
        // update cached_config to notify us when our config has changed.
        worker_iteration();
        thread::sleep(REFRESH_INTERVAL);
    }
}

fn worker_iteration() {
    let mut state = TUNABLES_WORKER_STATE
        .get()
        .expect("Tunables worker state uninitialised")
        .lock()
        .expect("Poisoned lock");

    let new_tunables = state.config_handle.get();
    if Some(&new_tunables) != state.old_tunables.as_ref() {
        debug!(
            state.logger,
            "Updating tunables, old: {}, new: {}",
            state
                .old_tunables
                .as_deref()
                .map_or_else(|| String::from("unknown"), log_tunables),
            log_tunables(&new_tunables),
        );
        match update_tunables(new_tunables.clone()) {
            Ok(_) => {
                state.old_tunables = Some(new_tunables);
            }
            Err(e) => {
                warn!(state.logger, "Failed to refresh tunables: {}", e);
                state.old_tunables = None;
            }
        }
    }
}

fn update_tunables(new_tunables: Arc<TunablesStruct>) -> Result<()> {
    let tunables = tunables();
    tunables.update_bools(&new_tunables.killswitches);
    tunables.update_ints(&new_tunables.ints);
    tunables.update_strings(&new_tunables.strings);

    if let Some(killswitches_by_repo) = &new_tunables.killswitches_by_repo {
        tunables.update_by_repo_bools(killswitches_by_repo);
    }

    if let Some(ints_by_repo) = &new_tunables.ints_by_repo {
        tunables.update_by_repo_ints(ints_by_repo);
    }

    if let Some(vec_of_strings_by_repo) = &new_tunables.vec_of_strings_by_repo {
        tunables.update_by_repo_vec_of_strings(vec_of_strings_by_repo);
    }
    Ok(())
}

/// A helper function to override tunables during a closure's execution.
/// This is useful for unit tests.
pub fn with_tunables<T>(new_tunables: MononokeTunables, f: impl FnOnce() -> T) -> T {
    TUNABLES_OVERRIDE.with(|t| *t.borrow_mut() = Some(Arc::new(new_tunables)));

    let res = f();

    TUNABLES_OVERRIDE.with(|tunables| *tunables.borrow_mut() = None);

    res
}

pub fn with_tunables_async<Out, Fut: Future<Output = Out> + Unpin>(
    new_tunables: MononokeTunables,
    fut: Fut,
) -> impl Future<Output = Out> {
    with_tunables_async_arc(Arc::new(new_tunables), fut)
}

pub fn with_tunables_async_arc<Out, Fut: Future<Output = Out> + Unpin>(
    new_tunables: Arc<MononokeTunables>,
    mut fut: Fut,
) -> impl Future<Output = Out> {
    poll_fn(move |cx| {
        TUNABLES_OVERRIDE.with(|t| *t.borrow_mut() = Some(new_tunables.clone()));

        let res = fut.poll_unpin(cx);

        TUNABLES_OVERRIDE.with(|tunables| *tunables.borrow_mut() = None);

        res
    })
}

pub fn override_tunables(new_tunables: Option<Arc<MononokeTunables>>) {
    TUNABLES_OVERRIDE.with(|t| *t.borrow_mut() = new_tunables);
}

#[cfg(test)]
mod test {
    use super::*;
    use maplit::hashmap;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    #[derive(Tunables, Default)]
    struct TestTunables {
        boolean: AtomicBool,
        num: AtomicI64,
        string: TunableString,

        repobool: TunableBoolByRepo,
        repobool2: TunableBoolByRepo,

        repoint: TunableI64ByRepo,
        repoint2: TunableI64ByRepo,

        repostr: TunableStringByRepo,
        repostr2: TunableStringByRepo,

        repovecofstrings: TunableVecOfStringsByRepo,
    }

    #[derive(Tunables, Default)]
    struct EmptyTunables {}

    fn s(a: &str) -> String {
        a.to_string()
    }

    #[test]
    fn test_override_tunables() {
        assert_eq!(tunables().get_wishlist_write_qps(), 0);

        let res = with_tunables(
            MononokeTunables {
                wishlist_write_qps: AtomicI64::new(2),
                ..MononokeTunables::default()
            },
            || tunables().get_wishlist_write_qps(),
        );

        assert_eq!(res, 2);
        assert_eq!(tunables().get_wishlist_write_qps(), 0);
    }

    #[test]
    fn test_empty_tunables() {
        let bools = HashMap::new();
        let ints = HashMap::new();
        let empty = EmptyTunables::default();

        empty.update_bools(&bools);
        empty.update_ints(&ints);
        empty.update_strings(&HashMap::new());
    }

    #[test]
    fn test_update_bool() {
        let mut d = HashMap::new();
        d.insert(s("boolean"), true);

        let test = TestTunables::default();
        assert_eq!(test.get_boolean(), false);
        test.update_bools(&d);
        assert_eq!(test.get_boolean(), true);
    }

    #[test]
    fn test_revert_update() {
        // booleans
        let mut d = hashmap! {};
        d.insert(s("boolean"), true);

        let test = TestTunables::default();
        assert_eq!(test.get_boolean(), false);
        test.update_bools(&d);
        assert_eq!(test.get_boolean(), true);

        test.update_bools(&hashmap! {});
        assert_eq!(test.get_boolean(), false);

        // ints
        let test = TestTunables::default();
        test.update_ints(&hashmap! { s("num") => 1});
        assert_eq!(test.get_num(), 1);

        test.update_ints(&hashmap! {});
        assert_eq!(test.get_num(), 0);

        // strings
        let test = TestTunables::default();
        test.update_strings(&hashmap! { s("string") => s("string")});
        assert_eq!(test.get_string(), Arc::new(s("string")));

        test.update_strings(&hashmap! {});
        assert_eq!(test.get_string(), Arc::new(s("")));

        // by repo bools
        assert_eq!(test.get_by_repo_repobool("repo"), None);
        assert_eq!(test.get_by_repo_repobool("repo2"), None);

        test.update_by_repo_bools(&hashmap! {
            s("repo") => hashmap! {
                s("repobool") => true,
            },
            s("repo2") => hashmap! {
                s("repobool") => false,
            }
        });
        assert_eq!(test.get_by_repo_repobool("repo"), Some(true));
        assert_eq!(test.get_by_repo_repobool("repo2"), Some(false));

        test.update_by_repo_bools(&hashmap! {});
        assert_eq!(test.get_by_repo_repobool("repo"), None);
        assert_eq!(test.get_by_repo_repobool("repo2"), None);
    }

    #[test]
    fn test_update_int() {
        let mut d = HashMap::new();
        d.insert(s("num"), 10);

        let test = TestTunables::default();
        assert_eq!(test.get_num(), 0);
        test.update_ints(&d);
        assert_eq!(test.get_num(), 10);
    }

    #[test]
    fn test_missing_int() {
        let mut d = HashMap::new();
        d.insert(s("missing"), 10);

        let test = TestTunables::default();
        assert_eq!(test.get_num(), 0);
        test.update_ints(&d);
        assert_eq!(test.get_num(), 0);
    }

    #[test]
    fn update_string() {
        let mut d = HashMap::new();
        d.insert(s("string"), s("value"));

        let test = TestTunables::default();
        assert_eq!(test.get_string().as_str(), "");
        test.update_strings(&d);
        assert_eq!(test.get_string().as_str(), "value");
    }

    #[test]
    fn update_by_repo_bool() {
        let test = TestTunables::default();

        assert_eq!(test.get_by_repo_repobool("repo"), None);
        assert_eq!(test.get_by_repo_repobool("repo2"), None);

        test.update_by_repo_bools(&hashmap! {
            s("repo") => hashmap! {
                s("repobool") => true,
            },
            s("repo2") => hashmap! {
                s("repobool") => true,
            }
        });
        assert_eq!(test.get_by_repo_repobool("repo"), Some(true));
        assert_eq!(test.get_by_repo_repobool("repo2"), Some(true));

        test.update_by_repo_bools(&hashmap! {
            s("repo") => hashmap! {
                s("repobool") => true,
            }
        });
        assert_eq!(test.get_by_repo_repobool("repo2"), None);

        test.update_by_repo_bools(&hashmap! {
            s("repo") => hashmap! {
                s("repobool") => false,
            }
        });
        assert_eq!(test.get_by_repo_repobool("repo"), Some(false));
    }

    #[test]
    fn update_by_repo_two_bools() {
        let test = TestTunables::default();
        assert_eq!(test.get_by_repo_repobool("repo"), None);
        assert_eq!(test.get_by_repo_repobool2("repo"), None);

        test.update_by_repo_bools(&hashmap! {
            s("repo") => hashmap! {
                s("repobool") => true,
                s("repobool2") => true,
            }
        });

        assert_eq!(test.get_by_repo_repobool("repo"), Some(true));
        assert_eq!(test.get_by_repo_repobool2("repo"), Some(true));

        test.update_by_repo_bools(&hashmap! {
            s("repo") => hashmap! {
                s("repobool") => true,
                s("repobool2") => false,
            }
        });

        assert_eq!(test.get_by_repo_repobool("repo"), Some(true));
        assert_eq!(test.get_by_repo_repobool2("repo"), Some(false));
    }

    #[test]
    fn update_by_repo_str() {
        let test = TestTunables::default();

        assert_eq!(test.get_by_repo_repostr("repo"), None);
        assert_eq!(test.get_by_repo_repostr("repo2"), None);

        test.update_by_repo_strings(&hashmap! {
            s("repo") => hashmap! {
                s("repostr") => s("hello"),
            },
            s("repo2") => hashmap! {
                s("repostr") => s("world"),
            },
        });
        assert_eq!(test.get_by_repo_repostr("repo"), Some(s("hello")));
        assert_eq!(test.get_by_repo_repostr("repo2"), Some(s("world")));

        test.update_by_repo_strings(&hashmap! {
            s("repo") => hashmap! {
                s("repostr") => s("hello2"),
            },
        });
        assert_eq!(test.get_by_repo_repostr("repo"), Some(s("hello2")));
        assert_eq!(test.get_by_repo_repostr("repo2"), None);
    }

    #[test]
    fn update_by_repo_two_strs() {
        let test = TestTunables::default();
        assert_eq!(test.get_by_repo_repostr("repo"), None);
        assert_eq!(test.get_by_repo_repostr2("repo"), None);

        test.update_by_repo_strings(&hashmap! {
            s("repo") => hashmap! {
                s("repostr") => s("hello"),
                s("repostr2") => s("world"),
            }
        });

        assert_eq!(test.get_by_repo_repostr("repo"), Some(s("hello")));
        assert_eq!(test.get_by_repo_repostr2("repo"), Some(s("world")));

        test.update_by_repo_strings(&hashmap! {
            s("repo") => hashmap! {
                s("repostr") => s("hello2"),
            }
        });

        assert_eq!(test.get_by_repo_repostr("repo"), Some(s("hello2")));
        assert_eq!(test.get_by_repo_repostr2("repo"), None);
    }

    #[test]
    fn update_by_repo_int() {
        let test = TestTunables::default();

        assert_eq!(test.get_by_repo_repoint("repo"), None);
        assert_eq!(test.get_by_repo_repoint("repo2"), None);

        test.update_by_repo_ints(&hashmap! {
            s("repo") => hashmap! {
                s("repoint") => 1,
            },
            s("repo2") => hashmap! {
                s("repoint") => 2,
            },
        });
        assert_eq!(test.get_by_repo_repoint("repo"), Some(1));
        assert_eq!(test.get_by_repo_repoint("repo2"), Some(2));

        test.update_by_repo_ints(&hashmap! {
            s("repo") => hashmap! {
                s("repoint") => 3,
            },
        });
        assert_eq!(test.get_by_repo_repoint("repo"), Some(3));
        assert_eq!(test.get_by_repo_repoint("repo2"), None);
    }

    #[test]
    fn update_by_repo_two_ints() {
        let test = TestTunables::default();
        assert_eq!(test.get_by_repo_repoint("repo"), None);
        assert_eq!(test.get_by_repo_repoint2("repo"), None);

        test.update_by_repo_ints(&hashmap! {
            s("repo") => hashmap! {
                s("repoint") => 1,
                s("repoint2") => 2,
            }
        });

        assert_eq!(test.get_by_repo_repoint("repo"), Some(1));
        assert_eq!(test.get_by_repo_repoint2("repo"), Some(2));

        test.update_by_repo_ints(&hashmap! {
            s("repo") => hashmap! {
                s("repoint") => 3
            }
        });

        assert_eq!(test.get_by_repo_repoint("repo"), Some(3));
        assert_eq!(test.get_by_repo_repoint2("repo"), None);
    }

    #[test]
    fn update_by_repo_vec_of_strings() {
        let test = TestTunables::default();
        assert_eq!(test.get_by_repo_repovecofstrings("repo"), None);

        test.update_by_repo_vec_of_strings(&hashmap! {
            s("repo") => hashmap! {
                s("unrelated") => vec![s("val1"), s("val2")],
            }
        });
        assert_eq!(test.get_by_repo_repovecofstrings("repo"), None);

        test.update_by_repo_vec_of_strings(&hashmap! {
            s("repo") => hashmap! {
                s("repovecofstrings") => vec![s("val1"), s("val2")],
            }
        });

        assert_eq!(
            test.get_by_repo_repovecofstrings("repo"),
            Some(vec![s("val1"), s("val2")])
        );
    }

    #[fbinit::test]
    async fn test_with_tunables_async(_fb: fbinit::FacebookInit) {
        let res = with_tunables_async(
            MononokeTunables {
                wishlist_write_qps: AtomicI64::new(2),
                ..MononokeTunables::default()
            },
            async { tunables().get_wishlist_write_qps() }.boxed(),
        )
        .await;

        assert_eq!(res, 2);
    }
}
