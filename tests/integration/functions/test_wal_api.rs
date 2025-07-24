use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, rng_from_time, TempDatabase};

#[test]
fn test_wal_frame_count() {
    let db = TempDatabase::new_empty(false);
    let conn = db.connect_limbo();
    assert_eq!(conn.wal_frame_count().unwrap(), 0);
    conn.execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    assert_eq!(conn.wal_frame_count().unwrap(), 2);
    conn.execute("INSERT INTO t VALUES (10, 10), (5, 1)")
        .unwrap();
    assert_eq!(conn.wal_frame_count().unwrap(), 3);
    conn.execute("INSERT INTO t VALUES (1024, randomblob(4096 * 10))")
        .unwrap();
    assert_eq!(conn.wal_frame_count().unwrap(), 15);
}

#[test]
fn test_wal_frame_transfer_no_schema_changes() {
    let db1 = TempDatabase::new_empty(false);
    let conn1 = db1.connect_limbo();
    let db2 = TempDatabase::new_empty(false);
    let conn2 = db2.connect_limbo();
    conn1
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn2
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn1
        .execute("INSERT INTO t VALUES (10, 10), (5, 1)")
        .unwrap();
    conn1
        .execute("INSERT INTO t VALUES (1024, randomblob(4096 * 10))")
        .unwrap();
    assert_eq!(conn1.wal_frame_count().unwrap(), 15);
    let mut frame = [0u8; 24 + 4096];
    conn2.wal_insert_begin().unwrap();
    for frame_id in 1..=conn1.wal_frame_count().unwrap() as u32 {
        let c = conn1.wal_get_frame(frame_id, &mut frame).unwrap();
        db1.io.wait_for_completion(c).unwrap();
        conn2.wal_insert_frame(frame_id, &frame).unwrap();
    }
    conn2.wal_insert_end().unwrap();
    assert_eq!(conn2.wal_frame_count().unwrap(), 15);
    assert_eq!(
        limbo_exec_rows(&db2, &conn2, "SELECT x, length(y) FROM t"),
        vec![
            vec![Value::Integer(5), Value::Integer(1)],
            vec![Value::Integer(10), Value::Integer(2)],
            vec![Value::Integer(1024), Value::Integer(40960)],
        ]
    );
}

#[test]
fn test_wal_frame_transfer_schema_changes() {
    let db1 = TempDatabase::new_empty(false);
    let conn1 = db1.connect_limbo();
    let db2 = TempDatabase::new_empty(false);
    let conn2 = db2.connect_limbo();
    conn1
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn1
        .execute("INSERT INTO t VALUES (10, 10), (5, 1)")
        .unwrap();
    conn1
        .execute("INSERT INTO t VALUES (1024, randomblob(4096 * 10))")
        .unwrap();
    assert_eq!(conn1.wal_frame_count().unwrap(), 15);
    let mut frame = [0u8; 24 + 4096];
    conn2.wal_insert_begin().unwrap();
    for frame_id in 1..=conn1.wal_frame_count().unwrap() as u32 {
        let c = conn1.wal_get_frame(frame_id, &mut frame).unwrap();
        db1.io.wait_for_completion(c).unwrap();
        conn2.wal_insert_frame(frame_id, &frame).unwrap();
    }
    conn2.wal_insert_end().unwrap();
    assert_eq!(conn2.wal_frame_count().unwrap(), 15);
    assert_eq!(
        limbo_exec_rows(&db2, &conn2, "SELECT x, length(y) FROM t"),
        vec![
            vec![Value::Integer(5), Value::Integer(1)],
            vec![Value::Integer(10), Value::Integer(2)],
            vec![Value::Integer(1024), Value::Integer(40960)],
        ]
    );
}

#[test]
fn test_wal_frame_transfer_no_schema_changes_rollback() {
    let db1 = TempDatabase::new_empty(false);
    let conn1 = db1.connect_limbo();
    let db2 = TempDatabase::new_empty(false);
    let conn2 = db2.connect_limbo();
    conn1
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn2
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn1
        .execute("INSERT INTO t VALUES (1024, randomblob(4096 * 10))")
        .unwrap();
    assert_eq!(conn1.wal_frame_count().unwrap(), 14);
    let mut frame = [0u8; 24 + 4096];
    conn2.wal_insert_begin().unwrap();
    for frame_id in 1..=(conn1.wal_frame_count().unwrap() as u32 - 1) {
        let c = conn1.wal_get_frame(frame_id, &mut frame).unwrap();
        db1.io.wait_for_completion(c).unwrap();
        conn2.wal_insert_frame(frame_id, &frame).unwrap();
    }
    conn2.wal_insert_end().unwrap();
    assert_eq!(conn2.wal_frame_count().unwrap(), 2);
    assert_eq!(
        limbo_exec_rows(&db2, &conn2, "SELECT x, length(y) FROM t"),
        vec![] as Vec<Vec<rusqlite::types::Value>>
    );
    conn2.execute("CREATE TABLE q(x)").unwrap();
    conn2
        .execute("INSERT INTO q VALUES (randomblob(4096 * 10))")
        .unwrap();
    assert_eq!(
        limbo_exec_rows(&db2, &conn2, "SELECT x, LENGTH(y) FROM t"),
        vec![] as Vec<Vec<rusqlite::types::Value>>
    );
}

#[test]
fn test_wal_frame_transfer_schema_changes_rollback() {
    let db1 = TempDatabase::new_empty(false);
    let conn1 = db1.connect_limbo();
    let db2 = TempDatabase::new_empty(false);
    let conn2 = db2.connect_limbo();
    conn1
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn1
        .execute("INSERT INTO t VALUES (1024, randomblob(4096 * 10))")
        .unwrap();
    assert_eq!(conn1.wal_frame_count().unwrap(), 14);
    let mut frame = [0u8; 24 + 4096];
    conn2.wal_insert_begin().unwrap();
    for frame_id in 1..=(conn1.wal_frame_count().unwrap() as u32 - 1) {
        let c = conn1.wal_get_frame(frame_id, &mut frame).unwrap();
        db1.io.wait_for_completion(c).unwrap();
        conn2.wal_insert_frame(frame_id, &frame).unwrap();
    }
    conn2.wal_insert_end().unwrap();
    assert_eq!(conn2.wal_frame_count().unwrap(), 2);
    assert_eq!(
        limbo_exec_rows(&db2, &conn2, "SELECT x, length(y) FROM t"),
        vec![] as Vec<Vec<rusqlite::types::Value>>
    );
    conn2.execute("CREATE TABLE q(x)").unwrap();
    conn2
        .execute("INSERT INTO q VALUES (randomblob(4096 * 10))")
        .unwrap();
    assert_eq!(
        limbo_exec_rows(&db2, &conn2, "SELECT x, LENGTH(y) FROM t"),
        vec![] as Vec<Vec<rusqlite::types::Value>>
    );
}

#[test]
fn test_wal_frame_conflict() {
    let db1 = TempDatabase::new_empty(false);
    let conn1 = db1.connect_limbo();
    let db2 = TempDatabase::new_empty(false);
    let conn2 = db2.connect_limbo();
    conn1
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn2
        .execute("CREATE TABLE q(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    assert_eq!(conn1.wal_frame_count().unwrap(), 2);
    let mut frame = [0u8; 24 + 4096];
    conn2.wal_insert_begin().unwrap();
    let c = conn1.wal_get_frame(1, &mut frame).unwrap();
    db1.io.wait_for_completion(c).unwrap();
    assert!(conn2.wal_insert_frame(1, &frame).is_err());
}

#[test]
fn test_wal_frame_far_away_write() {
    let db1 = TempDatabase::new_empty(false);
    let conn1 = db1.connect_limbo();
    let db2 = TempDatabase::new_empty(false);
    let conn2 = db2.connect_limbo();
    conn1
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn2
        .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    conn1
        .execute("INSERT INTO t VALUES (1024, randomblob(4096 * 10))")
        .unwrap();
    assert_eq!(conn1.wal_frame_count().unwrap(), 14);
    let mut frame = [0u8; 24 + 4096];
    conn2.wal_insert_begin().unwrap();

    let c = conn1.wal_get_frame(3, &mut frame).unwrap();
    db1.io.wait_for_completion(c).unwrap();
    conn2.wal_insert_frame(3, &frame).unwrap();

    let c = conn1.wal_get_frame(5, &mut frame).unwrap();
    db1.io.wait_for_completion(c).unwrap();
    assert!(conn2.wal_insert_frame(5, &frame).is_err());
}

#[test]
fn test_wal_frame_api_no_schema_changes_fuzz() {
    let (mut rng, _) = rng_from_time();
    for _ in 0..4 {
        let db1 = TempDatabase::new_empty(false);
        let conn1 = db1.connect_limbo();
        let db2 = TempDatabase::new_empty(false);
        let conn2 = db2.connect_limbo();
        conn1
            .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
            .unwrap();
        conn2
            .execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
            .unwrap();

        let seed = rng.next_u64();
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        println!("SEED: {seed}");

        let (mut size, mut synced_frame) = (0, conn2.wal_frame_count().unwrap());
        let mut commit_frames = vec![conn1.wal_frame_count().unwrap()];
        for _ in 0..256 {
            if rng.next_u32() % 10 != 0 {
                let key = rng.next_u32();
                let length = rng.next_u32() % (4 * 4096);
                let query = format!("INSERT INTO t VALUES ({key}, randomblob({length}))");
                conn1.execute(&query).unwrap();
                commit_frames.push(conn1.wal_frame_count().unwrap());
            } else {
                let last_frame = conn1.wal_frame_count().unwrap();
                let next_frame =
                    synced_frame + (rng.next_u32() as u64 % (last_frame - synced_frame + 1));
                let mut frame = [0u8; 24 + 4096];
                conn2.wal_insert_begin().unwrap();
                for frame_no in (synced_frame + 1)..=next_frame {
                    let c = conn1.wal_get_frame(frame_no as u32, &mut frame).unwrap();
                    db1.io.wait_for_completion(c).unwrap();
                    conn2.wal_insert_frame(frame_no as u32, &frame[..]).unwrap();
                }
                conn2.wal_insert_end().unwrap();
                for (i, committed) in commit_frames.iter().enumerate() {
                    if *committed <= next_frame {
                        size = size.max(i);
                        synced_frame = *committed;
                    }
                }
                if rng.next_u32() % 10 == 0 {
                    synced_frame = rng.next_u32() as u64 % synced_frame;
                }
                assert_eq!(
                    limbo_exec_rows(&db2, &conn2, "SELECT COUNT(*) FROM t"),
                    vec![vec![Value::Integer(size as i64)]]
                );
            }
        }
    }
}
