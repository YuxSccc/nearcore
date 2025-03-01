pub use crate::config::{init_configs, load_config, load_test_config, NearConfig, NEAR_BASE};
use crate::migrations::{
    migrate_12_to_13, migrate_18_to_19, migrate_19_to_20, migrate_22_to_23, migrate_23_to_24,
    migrate_24_to_25, migrate_30_to_31,
};
pub use crate::runtime::NightshadeRuntime;
pub use crate::shard_tracker::TrackedConfig;
use actix::{Actor, Addr, Arbiter};
use actix_rt::ArbiterHandle;
use actix_web;
use anyhow::Context;
use near_chain::ChainGenesis;
#[cfg(feature = "test_features")]
use near_client::AdversarialControls;
use near_client::{start_client, start_view_client, ClientActor, ViewClientActor};
use near_network::routing::start_routing_table_actor;
use near_network::test_utils::NetworkRecipient;
use near_network::PeerManagerActor;
use near_primitives::network::PeerId;
#[cfg(feature = "rosetta_rpc")]
use near_rosetta_rpc::start_rosetta_rpc;
#[cfg(feature = "performance_stats")]
use near_rust_allocator_proxy::reset_memory_usage_max;
use near_store::db::DBCol;
use near_store::db::RocksDB;
use near_store::migrations::{
    fill_col_outcomes_by_hash, fill_col_transaction_refcount, get_store_version, migrate_10_to_11,
    migrate_11_to_12, migrate_13_to_14, migrate_14_to_15, migrate_17_to_18, migrate_20_to_21,
    migrate_21_to_22, migrate_25_to_26, migrate_26_to_27, migrate_28_to_29, migrate_29_to_30,
    migrate_6_to_7, migrate_7_to_8, migrate_8_to_9, migrate_9_to_10, set_store_version,
};
use near_store::{create_store, create_store_with_config, Store, StoreConfig};
use near_telemetry::TelemetryActor;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{error, info, trace};

pub mod append_only_map;
pub mod config;
mod metrics;
pub mod migrations;
mod runtime;
mod shard_tracker;

const STORE_PATH: &str = "data";

pub fn store_path_exists<P: AsRef<Path>>(path: P) -> bool {
    fs::canonicalize(path).is_ok()
}

pub fn get_store_path(base_path: &Path) -> PathBuf {
    let mut store_path = base_path.to_owned();
    store_path.push(STORE_PATH);
    if store_path_exists(&store_path) {
        info!(target: "near", "Opening store database at {:?}", store_path);
    } else {
        info!(target: "near", "Did not find {:?} path, will be creating new store database", store_path);
    }
    store_path
}

pub fn get_default_home() -> PathBuf {
    if let Ok(near_home) = std::env::var("NEAR_HOME") {
        return near_home.into();
    }

    if let Some(mut home) = dirs::home_dir() {
        home.push(".near");
        return home;
    }

    PathBuf::default()
}

/// Returns the path of the DB checkpoint.
/// Default location is the same as the database location: `path`.
fn db_checkpoint_path(path: &Path, near_config: &NearConfig) -> PathBuf {
    let root_path =
        if let Some(db_migration_snapshot_path) = &near_config.config.db_migration_snapshot_path {
            assert!(
                db_migration_snapshot_path.is_absolute(),
                "'db_migration_snapshot_path' must be an absolute path to an existing directory."
            );
            db_migration_snapshot_path.clone()
        } else {
            path.to_path_buf()
        };
    root_path.join(DB_CHECKPOINT_NAME)
}

const DB_CHECKPOINT_NAME: &str = "db_migration_snapshot";

/// Creates a consistent DB checkpoint and returns its path.
/// By default it creates checkpoints in the DB directory, but can be overridden by the config.
fn create_db_checkpoint(path: &Path, near_config: &NearConfig) -> Result<PathBuf, anyhow::Error> {
    let checkpoint_path = db_checkpoint_path(path, near_config);
    if checkpoint_path.exists() {
        return Err(anyhow::anyhow!(
            "Detected an existing database migration snapshot: '{}'.\n\
             Probably a database migration got interrupted and your database is corrupted.\n\
             Please replace the contents of '{}' with data from that checkpoint, delete the checkpoint and try again.",
            checkpoint_path.display(),
            path.display()));
    }

    let db = RocksDB::new(path)?;
    let checkpoint = db.checkpoint()?;
    info!(target: "near", "Creating a database migration snapshot in '{}'", checkpoint_path.display());
    checkpoint.create_checkpoint(&checkpoint_path)?;
    info!(target: "near", "Created a database migration snapshot in '{}'", checkpoint_path.display());

    Ok(checkpoint_path)
}

/// Function checks current version of the database and applies migrations to the database.
pub fn apply_store_migrations(path: &Path, near_config: &NearConfig) {
    let db_version = get_store_version(path);
    if db_version > near_primitives::version::DB_VERSION {
        error!(target: "near", "DB version {} is created by a newer version of neard, please update neard or delete data", db_version);
        std::process::exit(1);
    }

    if db_version == near_primitives::version::DB_VERSION {
        return;
    }

    // Before starting a DB migration, create a consistent snapshot of the database. If a migration
    // fails, it can be used to quickly restore the database to its original state.
    let checkpoint_path = if near_config.config.use_db_migration_snapshot {
        match create_db_checkpoint(path, near_config) {
            Ok(checkpoint_path) => {
                info!(target: "near", "Created a DB checkpoint before a DB migration: '{}'. Please recover from this checkpoint if the migration gets interrupted.", checkpoint_path.display());
                Some(checkpoint_path)
            }
            Err(err) => {
                panic!(
                    "Failed to create a database migration snapshot:\n\
                     {}\n\
                     Please consider fixing this issue and retrying.\n\
                     You can change the location of database migration snapshots by adjusting `config.json`:\n\
                     \t\"db_migration_snapshot_path\": \"/absolute/path/to/existing/dir\",\n\
                     Alternatively, you can disable database migration snapshots in `config.json`:\n\
                     \t\"use_db_migration_snapshot\": false,\n\
                     ",
                    err
                );
            }
        }
    } else {
        None
    };

    // Add migrations here based on `db_version`.
    if db_version <= 1 {
        // version 1 => 2: add gc column
        // Does not need to do anything since open db with option `create_missing_column_families`
        // Nevertheless need to bump db version, because db_version 1 binary can't open db_version 2 db
        info!(target: "near", "Migrate DB from version 1 to 2");
        let store = create_store(path);
        set_store_version(&store, 2);
    }
    if db_version <= 2 {
        // version 2 => 3: add ColOutcomesByBlockHash + rename LastComponentNonce -> ColLastComponentNonce
        // The column number is the same, so we don't need additional updates
        info!(target: "near", "Migrate DB from version 2 to 3");
        let store = create_store(path);
        fill_col_outcomes_by_hash(&store);
        set_store_version(&store, 3);
    }
    if db_version <= 3 {
        // version 3 => 4: add ColTransactionRefCount
        info!(target: "near", "Migrate DB from version 3 to 4");
        let store = create_store(path);
        fill_col_transaction_refcount(&store);
        set_store_version(&store, 4);
    }
    if db_version <= 4 {
        info!(target: "near", "Migrate DB from version 4 to 5");
        // version 4 => 5: add ColProcessedBlockHeights
        // we don't need to backfill the old heights since at worst we will just process some heights
        // again.
        let store = create_store(path);
        set_store_version(&store, 5);
    }
    if db_version <= 5 {
        info!(target: "near", "Migrate DB from version 5 to 6");
        // version 5 => 6: add merge operator to ColState
        // we don't have merge records before so old storage works
        let store = create_store(path);
        set_store_version(&store, 6);
    }
    if db_version <= 6 {
        info!(target: "near", "Migrate DB from version 6 to 7");
        // version 6 => 7:
        // - make ColState use 8 bytes for refcount (change to merge operator)
        // - move ColTransactionRefCount into ColTransactions
        // - make ColReceiptIdToShardId refcounted
        migrate_6_to_7(path);
    }
    if db_version <= 7 {
        info!(target: "near", "Migrate DB from version 7 to 8");
        // version 7 => 8:
        // delete values in column `StateColParts`
        migrate_7_to_8(path);
    }
    if db_version <= 8 {
        info!(target: "near", "Migrate DB from version 8 to 9");
        // version 8 => 9:
        // Repair `ColTransactions`, `ColReceiptIdToShardId`
        migrate_8_to_9(path);
    }
    if db_version <= 9 {
        info!(target: "near", "Migrate DB from version 9 to 10");
        // version 9 => 10;
        // populate partial encoded chunks for chunks that exist in storage
        migrate_9_to_10(path, near_config.client_config.archive);
    }
    if db_version <= 10 {
        info!(target: "near", "Migrate DB from version 10 to 11");
        // version 10 => 11
        // Add final head
        migrate_10_to_11(path);
    }
    if db_version <= 11 {
        info!(target: "near", "Migrate DB from version 11 to 12");
        // version 11 => 12;
        // populate ColReceipts with existing receipts
        migrate_11_to_12(path);
    }
    if db_version <= 12 {
        info!(target: "near", "Migrate DB from version 12 to 13");
        // version 12 => 13;
        // migrate ColTransactionResult to fix the inconsistencies there
        migrate_12_to_13(path, near_config);
    }
    if db_version <= 13 {
        info!(target: "near", "Migrate DB from version 13 to 14");
        // version 13 => 14;
        // store versioned enums for shard chunks
        migrate_13_to_14(path);
    }
    if db_version <= 14 {
        info!(target: "near", "Migrate DB from version 14 to 15");
        // version 14 => 15;
        // Change ColOutcomesByBlockHash to be ordered within each shard
        migrate_14_to_15(path);
    }
    if db_version <= 15 {
        info!(target: "near", "Migrate DB from version 15 to 16");
        // version 15 => 16: add column for compiled contracts
        let store = create_store(path);
        set_store_version(&store, 16);
    }
    if db_version <= 16 {
        info!(target: "near", "Migrate DB from version 16 to 17");
        // version 16 => 17: add column for storing epoch validator info
        let store = create_store(path);
        set_store_version(&store, 17);
    }
    if db_version <= 17 {
        info!(target: "near", "Migrate DB from version 17 to 18");
        // version 17 => 18: add `hash` to `BlockInfo` and ColHeaderHashesByHeight
        migrate_17_to_18(path);
    }
    if db_version <= 18 {
        info!(target: "near", "Migrate DB from version 18 to 19");
        // version 18 => 19: populate ColEpochValidatorInfo for archival nodes
        migrate_18_to_19(path, near_config);
    }
    if db_version <= 19 {
        info!(target: "near", "Migrate DB from version 19 to 20");
        // version 19 => 20: fix execution outcome
        migrate_19_to_20(path, near_config);
    }
    if db_version <= 20 {
        info!(target: "near", "Migrate DB from version 20 to 21");
        // version 20 => 21: delete genesis json hash due to change in Genesis::json_hash function
        migrate_20_to_21(path);
    }
    if db_version <= 21 {
        info!(target: "near", "Migrate DB from version 21 to 22");
        // version 21 => 22: rectify inflation: add `timestamp` to `BlockInfo`
        migrate_21_to_22(path);
    }
    if db_version <= 22 {
        info!(target: "near", "Migrate DB from version 22 to 23");
        migrate_22_to_23(path, near_config);
    }
    if db_version <= 23 {
        info!(target: "near", "Migrate DB from version 23 to 24");
        migrate_23_to_24(path, near_config);
    }
    if db_version <= 24 {
        info!(target: "near", "Migrate DB from version 24 to 25");
        migrate_24_to_25(path);
    }
    if db_version <= 25 {
        info!(target: "near", "Migrate DB from version 25 to 26");
        migrate_25_to_26(path);
    }
    if db_version <= 26 {
        info!(target: "near", "Migrate DB from version 26 to 27");
        migrate_26_to_27(path, near_config.client_config.archive);
    }
    if db_version <= 27 {
        // version 27 => 28: add ColStateChangesForSplitStates
        // Does not need to do anything since open db with option `create_missing_column_families`
        // Nevertheless need to bump db version, because db_version 1 binary can't open db_version 2 db
        info!(target: "near", "Migrate DB from version 27 to 28");
        let store = create_store(path);
        set_store_version(&store, 28);
    }
    if db_version <= 28 {
        // version 28 => 29: delete ColNextBlockWithNewChunk, ColLastBlockWithNewChunk
        info!(target: "near", "Migrate DB from version 28 to 29");
        migrate_28_to_29(path);
    }
    if db_version <= 29 {
        // version 29 => 30: migrate all structures that use ValidatorStake to versionized version
        info!(target: "near", "Migrate DB from version 29 to 30");
        migrate_29_to_30(path);
    }
    if db_version <= 30 {
        // version 30 => 31: recompute block ordinal due to a bug fixed in #5761
        info!(target: "near", "Migrate DB from version 30 to 31");
        migrate_30_to_31(path, &near_config);
    }

    #[cfg(feature = "nightly_protocol")]
    {
        let store = create_store(&path);

        // set some dummy value to avoid conflict with other migrations from nightly features
        set_store_version(&store, 10000);
    }

    #[cfg(not(feature = "nightly_protocol"))]
    {
        let db_version = get_store_version(path);
        debug_assert_eq!(db_version, near_primitives::version::DB_VERSION);
    }

    // DB migration was successful, remove the checkpoint to avoid it taking up precious disk space.
    if let Some(checkpoint_path) = checkpoint_path {
        info!(target: "near", "Deleting the database migration snapshot at '{}'", checkpoint_path.display());
        match std::fs::remove_dir_all(&checkpoint_path) {
            Ok(_) => {
                info!(target: "near", "Deleted the database migration snapshot at '{}'", checkpoint_path.display());
            }
            Err(err) => {
                error!(
                    "Failed to delete the database migration snapshot at '{}'.\n\
                    \tError: {:#?}.\n\
                    \n\
                    Please delete the database migration snapshot manually before the next start of the node.",
                    checkpoint_path.display(),
                    err);
            }
        }
    }
}

pub fn init_and_migrate_store(home_dir: &Path, near_config: &NearConfig) -> Store {
    let path = get_store_path(home_dir);
    let store_exists = store_path_exists(&path);
    if store_exists {
        apply_store_migrations(&path, near_config);
    }
    let store = create_store_with_config(
        &path,
        StoreConfig {
            read_only: false,
            enable_statistics: near_config.config.enable_rocksdb_statistics,
        },
    );
    if !store_exists {
        set_store_version(&store, near_primitives::version::DB_VERSION);
    }
    store
}

pub struct NearNode {
    pub client: Addr<ClientActor>,
    pub view_client: Addr<ViewClientActor>,
    pub arbiters: Vec<ArbiterHandle>,
    pub rpc_servers: Vec<(&'static str, actix_web::dev::Server)>,
}

pub fn start_with_config(home_dir: &Path, config: NearConfig) -> Result<NearNode, anyhow::Error> {
    start_with_config_and_synchronization(home_dir, config, None)
}

pub fn start_with_config_and_synchronization(
    home_dir: &Path,
    config: NearConfig,
    // 'shutdown_signal' will notify the corresponding `oneshot::Receiver` when an instance of
    // `ClientActor` gets dropped.
    shutdown_signal: Option<oneshot::Sender<()>>,
) -> Result<NearNode, anyhow::Error> {
    let store = init_and_migrate_store(home_dir, &config);

    let runtime = Arc::new(NightshadeRuntime::with_config(
        home_dir,
        store.clone(),
        &config,
        config.client_config.trie_viewer_state_size_limit,
        config.client_config.max_gas_burnt_view,
    ));

    let telemetry = TelemetryActor::new(config.telemetry_config.clone()).start();
    let chain_genesis = ChainGenesis::from(&config.genesis);

    let node_id = PeerId::new(config.network_config.public_key.clone().into());
    let network_adapter = Arc::new(NetworkRecipient::default());
    #[cfg(feature = "test_features")]
    let adv =
        Arc::new(std::sync::RwLock::new(AdversarialControls::new(config.client_config.archive)));

    let view_client = start_view_client(
        config.validator_signer.as_ref().map(|signer| signer.validator_id().clone()),
        chain_genesis.clone(),
        runtime.clone(),
        network_adapter.clone(),
        config.client_config.clone(),
        #[cfg(feature = "test_features")]
        adv.clone(),
    );
    let (client_actor, client_arbiter_handle) = start_client(
        config.client_config,
        chain_genesis,
        runtime,
        node_id,
        network_adapter.clone(),
        config.validator_signer,
        telemetry,
        shutdown_signal,
        #[cfg(feature = "test_features")]
        adv.clone(),
    );

    #[allow(unused_mut)]
    let mut rpc_servers = Vec::new();
    let arbiter = Arbiter::new();
    let client_actor1 = client_actor.clone().recipient();
    let view_client1 = view_client.clone().recipient();
    config.network_config.verify().with_context(|| "start_with_config")?;
    let network_config = config.network_config;
    let routing_table_addr =
        start_routing_table_actor(PeerId::new(network_config.public_key.clone()), store.clone());
    #[cfg(all(feature = "json_rpc", feature = "test_features"))]
    let routing_table_addr2 = routing_table_addr.clone();
    let network_actor = PeerManagerActor::start_in_arbiter(&arbiter.handle(), move |_ctx| {
        PeerManagerActor::new(
            store,
            network_config,
            client_actor1,
            view_client1,
            routing_table_addr,
        )
        .unwrap()
    });

    #[cfg(feature = "json_rpc")]
    if let Some(rpc_config) = config.rpc_config {
        rpc_servers.extend_from_slice(&near_jsonrpc::start_http(
            rpc_config,
            config.genesis.config.clone(),
            client_actor.clone(),
            view_client.clone(),
            #[cfg(feature = "test_features")]
            network_actor.clone(),
            #[cfg(feature = "test_features")]
            routing_table_addr2,
        ));
    }

    #[cfg(feature = "rosetta_rpc")]
    if let Some(rosetta_rpc_config) = config.rosetta_rpc_config {
        rpc_servers.push((
            "Rosetta RPC",
            start_rosetta_rpc(
                rosetta_rpc_config,
                Arc::new(config.genesis.clone()),
                client_actor.clone(),
                view_client.clone(),
            ),
        ));
    }

    network_adapter.set_recipient(network_actor.recipient());

    rpc_servers.shrink_to_fit();

    trace!(target: "diagnostic", key="log", "Starting NEAR node with diagnostic activated");

    // We probably reached peak memory once on this thread, we want to see when it happens again.
    #[cfg(feature = "performance_stats")]
    reset_memory_usage_max();

    Ok(NearNode {
        client: client_actor,
        view_client,
        rpc_servers,
        arbiters: vec![client_arbiter_handle, arbiter.handle()],
    })
}

pub struct RecompressOpts {
    pub dest_dir: PathBuf,
    pub keep_partial_chunks: bool,
    pub keep_invalid_chunks: bool,
    pub keep_trie_changes: bool,
}

pub fn recompress_storage(home_dir: &Path, opts: RecompressOpts) -> anyhow::Result<()> {
    use strum::IntoEnumIterator;

    let config_path = home_dir.join(config::CONFIG_FILENAME);
    let archive = config::Config::from_file(&config_path)
        .map_err(|err| anyhow::anyhow!("{}: {}", config_path.display(), err))?
        .archive;
    let mut skip_columns = Vec::new();
    if archive && !opts.keep_partial_chunks {
        skip_columns.push(near_store::db::DBCol::ColPartialChunks);
    }
    if archive && !opts.keep_invalid_chunks {
        skip_columns.push(near_store::db::DBCol::ColInvalidChunks);
    }
    if archive && !opts.keep_trie_changes {
        skip_columns.push(near_store::db::DBCol::ColTrieChanges);
    }

    // We’re configuring each RocksDB to use 512 file descriptors.  Make sure we
    // can open that many by ensuring nofile limit is large enough to give us
    // some room to spare over 1024 file descriptors.
    let (soft, hard) = rlimit::Resource::NOFILE
        .get()
        .map_err(|err| anyhow::anyhow!("getrlimit: NOFILE: {}", err))?;
    // We’re configuring RocksDB to use max file descriptor limit of 512.  We’re
    // opening two databases and need some descriptors to spare thus 3*512.
    if soft < 3 * 512 {
        rlimit::Resource::NOFILE
            .set(3 * 512, hard)
            .map_err(|err| anyhow::anyhow!("setrlimit: NOFILE: {}", err))?;
    }

    let src_dir = home_dir.join(STORE_PATH);
    anyhow::ensure!(
        store_path_exists(&src_dir),
        "{}: source storage doesn’t exist",
        src_dir.display()
    );
    let db_version = get_store_version(&src_dir);
    anyhow::ensure!(
        db_version == near_primitives::version::DB_VERSION,
        "{}: expected DB version {} but got {}",
        src_dir.display(),
        near_primitives::version::DB_VERSION,
        db_version
    );

    anyhow::ensure!(
        !store_path_exists(&opts.dest_dir),
        "{}: directory already exists",
        opts.dest_dir.display()
    );

    info!(target: "recompress", src = %src_dir.display(), dest = %opts.dest_dir.display(), "Recompressing database");
    let src_store = create_store_with_config(
        &src_dir,
        StoreConfig { read_only: true, enable_statistics: false },
    );

    let final_head_height = if skip_columns.contains(&DBCol::ColPartialChunks) {
        let tip: Option<near_primitives::block::Tip> =
            src_store.get_ser(DBCol::ColBlockMisc, near_store::FINAL_HEAD_KEY)?;
        anyhow::ensure!(
            tip.is_some(),
            "{}: missing {}; is this a freshly set up node? note that recompress_storage makes no sense on those",
            src_dir.display(),
            std::str::from_utf8(near_store::FINAL_HEAD_KEY).unwrap(),
        );
        tip.map(|tip| tip.height)
    } else {
        None
    };

    let dst_store = create_store(&opts.dest_dir);

    const BATCH_SIZE_BYTES: u64 = 150_000_000;

    for column in DBCol::iter() {
        let skip = skip_columns.contains(&column);
        info!(
            target: "recompress",
            column_id = column as usize,
            %column,
            "{}",
            if skip { "Clearing  " } else { "Processing" }
        );
        if skip {
            continue;
        }

        let mut store_update = dst_store.store_update();
        let mut total_written: u64 = 0;
        let mut batch_written: u64 = 0;
        let mut count_keys: u64 = 0;
        for (key, value) in src_store.iter_without_rc_logic(column) {
            store_update.set(column, &key, &value);
            total_written += value.len() as u64;
            batch_written += value.len() as u64;
            count_keys += 1;
            if batch_written >= BATCH_SIZE_BYTES {
                store_update.commit()?;
                info!(
                    target: "recompress",
                    column_id = column as usize,
                    %count_keys,
                    %total_written,
                    "Processing",
                );
                batch_written = 0;
                store_update = dst_store.store_update();
            }
        }
        info!(
            target: "recompress",
            column_id = column as usize,
            %count_keys,
            %total_written,
            "Done with "
        );
        store_update.commit()?;
    }

    // If we’re not keeping ColPartialChunks, update chunk tail to point to
    // current final block.  If we don’t do that, the gc will try to work its
    // way from the genesis even though chunks at those heights have been
    // deleted.
    if skip_columns.contains(&DBCol::ColPartialChunks) {
        let chunk_tail = final_head_height.unwrap();
        info!(target: "recompress", %chunk_tail, "Setting chunk tail");
        let mut store_update = dst_store.store_update();
        store_update.set_ser(DBCol::ColBlockMisc, near_store::CHUNK_TAIL_KEY, &chunk_tail)?;
        store_update.commit()?;
    }

    core::mem::drop(dst_store);
    core::mem::drop(src_store);

    info!(target: "recompress", dest_dir = ?opts.dest_dir, "Database recompressed");
    Ok(())
}
