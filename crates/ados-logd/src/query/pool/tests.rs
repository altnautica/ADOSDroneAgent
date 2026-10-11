use super::*;

/// Seed a minimal store so a read-only open succeeds against it.
fn seed(path: &std::path::Path) {
    let conn = db::open(path).unwrap();
    conn.execute(
        "INSERT INTO sessions (started_us, kind) VALUES (1, 'boot')",
        [],
    )
    .unwrap();
}

#[test]
fn checkout_reuses_a_parked_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path);
    let pool = ConnPool::new(path, DEFAULT_MAX_IDLE);

    // First checkout opens fresh; dropping it parks one connection.
    {
        let conn = pool.checkout().unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
    assert_eq!(pool.idle.lock().unwrap().len(), 1, "one parked after drop");

    // Second checkout reuses the parked one, leaving the idle set empty
    // while it is held.
    let conn = pool.checkout().unwrap();
    assert_eq!(pool.idle.lock().unwrap().len(), 0, "reused, not reopened");
    drop(conn);
    assert_eq!(pool.idle.lock().unwrap().len(), 1, "re-parked on drop");
}

#[test]
fn idle_set_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    seed(&path);
    let pool = ConnPool::new(path, 2);

    // Hold three concurrently (the pool never blocks a checkout), then drop
    // them all: only `max_idle` (2) park, the surplus is dropped.
    let a = pool.checkout().unwrap();
    let b = pool.checkout().unwrap();
    let c = pool.checkout().unwrap();
    drop(a);
    drop(b);
    drop(c);
    assert_eq!(pool.idle.lock().unwrap().len(), 2, "capped at max_idle");
}
