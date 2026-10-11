use super::*;
use crate::implementation::rocksdb::{DatabaseArguments, DatabaseEnv, DatabaseEnvKind};
use alloy_primitives::{b256, Bytes, B256};
use reth_db_api::{
    cursor::{DbCursorRO, DbDupCursorRO},
    database::Database,
    models::ClientVersion,
    table::Table,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_trie_common::{nested_trie::StorageNodeEntry, StoredNibbles, StoredNibblesSubKey};
use std::{sync::mpsc, thread, time::Duration};

type StageCheckpoint = <tables::StageCheckpoints as Table>::Value;

fn bounds(number: u64) -> RpcReadViewBounds {
    RpcReadViewBounds {
        block_number: number,
        block_hash: B256::repeat_byte(number as u8),
        next_tx_num: number * 2,
    }
}

fn open() -> (tempfile::TempDir, Arc<DatabaseEnv>) {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(
        DatabaseEnv::open(
            dir.path(),
            DatabaseEnvKind::RW,
            DatabaseArguments::new(ClientVersion::default()),
        )
        .unwrap(),
    );
    (dir, env)
}

fn write_three_stores(env: &DatabaseEnv, value: u8) {
    let tx = env.tx_mut().unwrap();
    tx.put::<tables::Metadata>("rpc-view-test".into(), vec![value]).unwrap();
    tx.put::<tables::AccountsTrieV2>(StoredNibbles::from(vec![1]), Bytes::from(vec![value]))
        .unwrap();
    tx.put::<tables::StoragesTrieV2>(
        b256!("0000000000000000000000000000000000000000000000000000000000000001"),
        StorageNodeEntry {
            path: StoredNibblesSubKey::from(vec![2]),
            node: Bytes::from(vec![value]),
        },
    )
    .unwrap();
    tx.commit().unwrap();
}

fn assert_three_stores(tx: &impl DbTx, value: u8) {
    assert_eq!(tx.get::<tables::Metadata>("rpc-view-test".into()).unwrap(), Some(vec![value]));
    assert_eq!(
        tx.get::<tables::AccountsTrieV2>(StoredNibbles::from(vec![1])).unwrap(),
        Some(Bytes::from(vec![value]))
    );
    assert_eq!(
        tx.cursor_dup_read::<tables::StoragesTrieV2>()
            .unwrap()
            .seek_by_key_subkey(
                b256!("0000000000000000000000000000000000000000000000000000000000000001"),
                StoredNibblesSubKey::from(vec![2]),
            )
            .unwrap()
            .unwrap()
            .node,
        Bytes::from(vec![value])
    );
}

#[test]
fn rpc_reads_published_three_store_view_while_write_barrier_is_held() {
    let (_dir, env) = open();
    assert!(env.rpc_read_lease().is_err());
    let mut write = env.consistent_write();
    write_three_stores(&env, 1);
    env.publish_rpc_view(bounds(1), true).unwrap();
    write.complete();
    drop(write);

    let old = env.tx_rpc(env.rpc_read_lease().unwrap()).unwrap();
    let mut write = env.consistent_write();
    write_three_stores(&env, 2);
    let (send, recv) = mpsc::channel();
    let rpc_env = env.clone();
    let reader = thread::spawn(move || {
        let tx = rpc_env.tx_rpc(rpc_env.rpc_read_lease().unwrap()).unwrap();
        assert_three_stores(&tx, 1);
        assert_eq!(tx.rpc_read_view_bounds(), Some(bounds(1)));
        send.send(()).unwrap();
    });
    let result = recv.recv_timeout(Duration::from_secs(5));
    env.publish_rpc_view(bounds(2), true).unwrap();
    write.complete();
    drop(write);
    reader.join().unwrap();
    result.unwrap();

    assert_three_stores(&old, 1);
    let new = env.tx_rpc(env.rpc_read_lease().unwrap()).unwrap();
    assert_three_stores(&new, 2);
    assert_eq!(new.rpc_read_view_bounds(), Some(bounds(2)));
}

#[test]
fn cursor_retains_maintenance_lease_after_transaction_drop() {
    let (_dir, env) = open();
    write_three_stores(&env, 1);
    env.publish_rpc_view(bounds(1), true).unwrap();
    let tx = env.tx_rpc(env.rpc_read_lease().unwrap()).unwrap();
    let mut cursor = tx.cursor_read::<tables::Metadata>().unwrap();
    let mut dup_cursor = tx.cursor_dup_read::<tables::StoragesTrieV2>().unwrap();
    drop(tx);
    assert!(env.rpc_maintenance(false).is_none());
    assert_eq!(cursor.get("rpc-view-test".into()).unwrap().unwrap().1, vec![1]);
    assert!(dup_cursor.first().unwrap().is_some());
    drop(cursor);
    assert!(env.rpc_maintenance(false).is_none());
    drop(dup_cursor);
    let mut maintenance = env.rpc_maintenance(false).unwrap();
    env.publish_rpc_view(bounds(1), true).unwrap();
    maintenance.complete();
    drop(maintenance);
    assert!(env.rpc_read_lease().is_ok());
}

#[test]
fn maintenance_closes_admission_and_waits_for_existing_lease() {
    let (_dir, env) = open();
    env.publish_rpc_view(bounds(1), true).unwrap();
    let lease = env.rpc_read_lease().unwrap();
    let maintenance_env = env.clone();
    let (send, recv) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut guard = maintenance_env.rpc_maintenance(true).unwrap();
        send.send(()).unwrap();
        maintenance_env.publish_rpc_view(bounds(1), true).unwrap();
        guard.complete();
    });
    {
        let mut state = env.rpc_views.state.lock();
        while !state.maintenance {
            assert!(!env
                .rpc_views
                .changed
                .wait_for(&mut state, Duration::from_secs(5))
                .timed_out());
        }
    }
    assert!(env.rpc_read_lease().is_err());
    assert!(matches!(recv.try_recv(), Err(mpsc::TryRecvError::Empty)));
    // A lease captured before closure may open its view while maintenance waits.
    drop(env.tx_rpc(lease).unwrap());
    recv.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert!(env.rpc_read_lease().is_ok());
}

#[test]
fn failed_write_and_unpublished_maintenance_never_reopen_old_view() {
    let (_dir, env) = open();
    env.publish_rpc_view(bounds(1), true).unwrap();
    drop(env.consistent_write());
    assert!(env.rpc_read_lease().is_err());
    env.publish_rpc_view(bounds(2), true).unwrap();
    assert!(env.rpc_read_lease().is_err());

    let mut guard = env.rpc_maintenance(true).unwrap();
    guard.complete();
    drop(guard);
    assert!(env.rpc_read_lease().is_err());
    let mut guard = env.rpc_maintenance(true).unwrap();
    let mut write = env.consistent_write();
    env.publish_rpc_view(bounds(2), true).unwrap();
    write.recovered();
    drop(write);
    guard.complete();
    drop(guard);
    assert!(env.rpc_read_lease().is_ok());
}

#[test]
fn failed_write_records_replay_floor_before_recovery_resets_checkpoints() {
    let (_dir, env) = open();
    write_three_stores(&env, 1);
    env.publish_rpc_view(bounds(6), true).unwrap();
    let old = env.tx_rpc(env.rpc_read_lease().unwrap()).unwrap();
    let delayed = env.rpc_read_lease().unwrap();
    let write = env.consistent_write();
    let tx = env.tx_mut().unwrap();
    let mut checkpoint = StageCheckpoint { block_number: 8, ..Default::default() };
    tx.put::<tables::StageCheckpoints>("MerkleExecute".into(), checkpoint).unwrap();
    tx.commit().unwrap();
    drop(write);
    assert!(env.tx_rpc(delayed).is_err());
    assert_three_stores(&old, 1);
    drop(old);

    let mut maintenance = env.rpc_maintenance(true).unwrap();
    let tx = env.tx_mut().unwrap();
    checkpoint.block_number = 6;
    tx.put::<tables::StageCheckpoints>("MerkleExecute".into(), checkpoint).unwrap();
    tx.commit().unwrap();
    env.publish_rpc_view(bounds(6), true).unwrap();
    maintenance.complete();
    drop(maintenance);
    assert!(env.rpc_read_lease().is_err());
    env.publish_rpc_view(bounds(8), true).unwrap();
    assert!(env.rpc_read_lease().is_err());
    env.publish_rpc_view(bounds(9), true).unwrap();
    assert!(env.rpc_read_lease().is_ok());
}

#[test]
fn foreign_database_lease_is_rejected() {
    let (_dir, env) = open();
    let (_other_dir, other) = open();
    env.publish_rpc_view(bounds(1), true).unwrap();
    other.publish_rpc_view(bounds(1), true).unwrap();
    assert!(env.tx_rpc(other.rpc_read_lease().unwrap()).is_err());
}

#[test]
fn interrupted_writer_does_not_release_an_active_maintenance_guard() {
    let (_dir, env) = open();
    env.publish_rpc_view(bounds(1), true).unwrap();
    let mut maintenance = env.rpc_maintenance(false).unwrap();
    env.publish_rpc_view(bounds(2), true).unwrap();
    drop(env.consistent_write());
    assert!(env.rpc_maintenance(false).is_none());
    maintenance.complete();
    drop(maintenance);
    assert!(env.rpc_read_lease().is_err());
}

#[test]
fn successful_retry_can_complete_the_same_maintenance_range() {
    let (_dir, env) = open();
    env.publish_rpc_view(bounds(1), true).unwrap();
    let mut maintenance = env.rpc_maintenance(false).unwrap();
    drop(env.consistent_write());
    assert!(env.rpc_maintenance(false).is_none());
    let mut recovered = env.consistent_write();
    env.publish_rpc_view(bounds(2), true).unwrap();
    recovered.recovered();
    drop(recovered);
    maintenance.complete();
    drop(maintenance);
    assert!(env.rpc_read_lease().is_ok());
}

#[test]
fn startup_watermark_survives_partial_recovery_and_allows_later_unwind() {
    let (_dir, env) = open();
    let manager = Arc::new(RpcViewManager::new(10));
    let view = |number| {
        PublishedRpcView::new(
            bounds(number),
            env.state_db.clone(),
            env.account_db.clone(),
            env.storage_db.clone(),
        )
    };
    let mut maintenance = manager.maintenance(true).unwrap();
    manager.publish(view(5), true);
    maintenance.complete();
    drop(maintenance);
    assert!(manager.lease().is_err());
    manager.publish(view(9), true);
    assert!(manager.lease().is_err());
    manager.publish(view(10), true);
    assert!(manager.lease().is_ok());
    let mut maintenance = manager.maintenance(true).unwrap();
    manager.publish(view(2), true);
    maintenance.complete();
    drop(maintenance);
    let lease = manager.lease().unwrap();
    assert_eq!(manager.view(&lease).unwrap().bounds, bounds(2));
}

#[test]
fn reopened_database_captures_body_tail_before_startup_recovery() {
    let (dir, env) = open();
    let tx = env.tx_mut().unwrap();
    tx.put::<tables::BlockBodyIndices>(12, <tables::BlockBodyIndices as Table>::Value::default())
        .unwrap();
    let checkpoint = StageCheckpoint { block_number: 6, ..Default::default() };
    tx.put::<tables::StageCheckpoints>("Execution".into(), checkpoint).unwrap();
    tx.commit().unwrap();
    drop(env);
    let env = DatabaseEnv::open(
        dir.path(),
        DatabaseEnvKind::RW,
        DatabaseArguments::new(ClientVersion::default()),
    )
    .unwrap();
    let tx = env.tx_mut().unwrap();
    tx.delete::<tables::BlockBodyIndices>(12, None).unwrap();
    tx.commit().unwrap();
    env.publish_rpc_view(bounds(11), true).unwrap();
    assert!(env.rpc_read_lease().is_err());
    env.publish_rpc_view(bounds(12), true).unwrap();
    assert!(env.rpc_read_lease().is_ok());
}

#[test]
fn startup_validation_is_required_until_a_verified_view_is_adopted() {
    let (_dir, env) = open();
    let manager = Arc::new(RpcViewManager::new(10));
    let view = || {
        PublishedRpcView::new(
            bounds(10),
            env.state_db.clone(),
            env.account_db.clone(),
            env.storage_db.clone(),
        )
    };
    assert!(!manager.requires_validation(9));
    assert!(manager.requires_validation(10));
    let mut maintenance = manager.maintenance(true).unwrap();
    manager.publish(view(), false);
    maintenance.complete();
    drop(maintenance);
    assert!(manager.lease().is_err());
    assert!(manager.requires_validation(10));

    manager.publish(view(), true);
    assert!(manager.lease().is_ok());
    assert!(!manager.requires_validation(10));
    assert!(!manager.requires_validation(11));
}

#[test]
fn publication_failure_preserves_execution_reads_and_retries_validation() {
    let (_dir, env) = open();
    write_three_stores(&env, 1);
    env.publish_rpc_view(bounds(1), true).unwrap();
    let old = env.tx_rpc(env.rpc_read_lease().unwrap()).unwrap();

    let mut write = env.consistent_write();
    write_three_stores(&env, 2);
    env.rpc_publication_failed();
    write.complete();
    drop(write);
    assert_three_stores(&env.tx_live().unwrap(), 2);
    assert_three_stores(&env.tx().unwrap(), 2);
    assert_three_stores(&old, 1);
    assert!(env.rpc_read_lease().is_err());
    assert!(env.rpc_read_view_requires_validation(2));

    env.publish_rpc_view(bounds(2), false).unwrap();
    assert!(env.rpc_read_lease().is_err());
    env.publish_rpc_view(bounds(2), true).unwrap();
    assert_three_stores(&env.tx_rpc(env.rpc_read_lease().unwrap()).unwrap(), 2);
    assert!(!env.rpc_read_view_requires_validation(2));
}

#[test]
fn successful_maintenance_with_failed_publication_can_reopen_on_a_later_block() {
    let (_dir, env) = open();
    env.publish_rpc_view(bounds(1), true).unwrap();
    let mut maintenance = env.rpc_maintenance(true).unwrap();
    let mut write = env.consistent_write();
    write_three_stores(&env, 2);
    env.rpc_publication_failed();
    write.complete();
    drop(write);
    maintenance.complete();
    drop(maintenance);
    assert!(env.rpc_read_lease().is_err());
    assert!(env.tx_live().is_ok());

    env.publish_rpc_view(bounds(2), true).unwrap();
    assert!(env.rpc_read_lease().is_ok());
}

#[test]
fn publication_failure_does_not_clear_an_actual_interrupted_write() {
    let (_dir, env) = open();
    env.publish_rpc_view(bounds(1), true).unwrap();
    let mut maintenance = env.rpc_maintenance(true).unwrap();
    drop(env.consistent_write());
    env.rpc_publication_failed();
    maintenance.complete();
    drop(maintenance);
    env.publish_rpc_view(bounds(2), true).unwrap();
    assert!(env.rpc_read_lease().is_err());
    assert!(env.tx_live().is_err());
}

#[test]
fn existing_genesis_body_without_checkpoints_requires_validation_after_reopen() {
    let (dir, env) = open();
    let tx = env.tx_mut().unwrap();
    tx.put::<tables::BlockBodyIndices>(0, <tables::BlockBodyIndices as Table>::Value::default())
        .unwrap();
    tx.commit().unwrap();
    drop(env);
    let env = DatabaseEnv::open(
        dir.path(),
        DatabaseEnvKind::RW,
        DatabaseArguments::new(ClientVersion::default()),
    )
    .unwrap();
    assert!(env.rpc_read_view_requires_validation(1));
    env.publish_rpc_view(bounds(0), true).unwrap();
    assert!(env.rpc_read_lease().is_err());
    assert!(env.rpc_read_view_requires_validation(1));
}
