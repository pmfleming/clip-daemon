//! Real-engine contract and capacity probe; all data is synthetic and temporary.
use anyhow::{Result, ensure};
use futures_lite::future::block_on;
use std::{path::Path, process::Command, time::Instant};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 3 {
        block_on(crash_writer(Path::new(&args[1]), args[2] == "commit"))?;
        unreachable!();
    }
    ensure!(args.len() == 1, "no arguments accepted");
    let root = tempfile::tempdir()?;
    let path = root.path().join("contract.db");
    block_on(contract(&path))?;
    for mode in ["commit", "uncommitted"] {
        ensure!(
            Command::new(std::env::current_exe()?)
                .arg(&path)
                .arg(mode)
                .status()?
                .success()
        );
        block_on(recover(&path))?;
    }
    println!("PASS: Turso 0.8.3 SQL, FTS, 16/64 MiB BLOB, crash recovery, checkpoint");
    Ok(())
}

async fn open(path: &Path) -> Result<(turso::Database, turso::Connection)> {
    let db = turso::Builder::new_local(path.to_str().unwrap())
        .with_io(turso::IoBackend::Syscall)
        .experimental_index_method(true)
        .experimental_vacuum(true)
        .build()
        .await?;
    let conn = db.connect()?;
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
        .await?;
    ensure!(scalar(&conn, "PRAGMA synchronous").await? == 2);
    ensure!(scalar(&conn, "PRAGMA foreign_keys").await? == 1);
    Ok((db, conn))
}

async fn scalar(c: &turso::Connection, sql: &str) -> Result<i64> {
    Ok(c.query(sql, ()).await?.next().await?.unwrap().get(0)?)
}

async fn contract(path: &Path) -> Result<()> {
    let (_db, c) = open(path).await?;
    c.execute_batch("CREATE TABLE entries(id INTEGER PRIMARY KEY, revision INTEGER NOT NULL CHECK(revision>0), favorite INTEGER NOT NULL CHECK(favorite IN(0,1)), sequence INTEGER NOT NULL UNIQUE, body TEXT NOT NULL) STRICT;
CREATE INDEX history_order ON entries(favorite DESC, sequence DESC);
CREATE TABLE payloads(id INTEGER PRIMARY KEY REFERENCES entries(id) ON DELETE CASCADE, bytes BLOB NOT NULL) STRICT;
CREATE INDEX text_fts ON entries USING fts(body);").await?;
    let mut insert = c
        .prepare("INSERT INTO entries VALUES(?1,1,0,?1,?2) RETURNING id")
        .await?;
    c.execute("BEGIN IMMEDIATE", ()).await?;
    for i in 1..=1000_i64 {
        let mut result = insert
            .query((i, format!("clipboard résumé token{i}")))
            .await?;
        ensure!(result.next().await?.unwrap().get::<i64>(0)? == i);
        ensure!(result.next().await?.is_none());
    }
    c.execute("COMMIT", ()).await?;
    ensure!(
        c.execute("INSERT INTO entries VALUES(1001,0,0,1001,'bad')", ())
            .await
            .is_err()
    );
    ensure!(
        c.execute("INSERT INTO entries VALUES(1001,1,9,1001,'bad')", ())
            .await
            .is_err()
    );
    ensure!(
        c.execute("INSERT INTO payloads VALUES(9999,x'01')", ())
            .await
            .is_err()
    );
    ensure!(
        c.execute("INSERT INTO payloads VALUES(1,'not a blob')", ())
            .await
            .is_err()
    );
    let count = scalar(
        &c,
        "SELECT count(*) FROM entries WHERE fts_match(body,'clipboard')",
    )
    .await?;
    ensure!(count == 1000);
    c.execute_batch("BEGIN IMMEDIATE; UPDATE entries SET body='changed' WHERE id=1; ROLLBACK;")
        .await?;
    ensure!(
        scalar(
            &c,
            "SELECT count(*) FROM entries WHERE fts_match(body,'token1')"
        )
        .await?
            == 1
    );
    let mut plan = c.query("EXPLAIN QUERY PLAN SELECT id FROM entries ORDER BY favorite DESC,sequence DESC LIMIT 40", ()).await?;
    let mut indexed = false;
    while let Some(row) = plan.next().await? {
        let detail: String = row.get(3)?;
        indexed |= detail.contains("history_order");
        println!("plan: {detail}");
    }
    ensure!(indexed, "history index not selected");
    drop(plan);
    for mib in [16, 64] {
        let body = vec![0x42; mib * 1024 * 1024];
        let started = Instant::now();
        c.execute(
            "INSERT OR REPLACE INTO payloads VALUES(1,?1)",
            [body.as_slice()],
        )
        .await?;
        let mut rows = c.query("SELECT bytes FROM payloads WHERE id=1", ()).await?;
        ensure!(rows.next().await?.unwrap().get::<Vec<u8>>(0)? == body);
        drop(rows);
        println!(
            "blob_mib={mib} commit_and_read_ms={}",
            started.elapsed().as_millis()
        );
    }
    let start = Instant::now();
    for _ in 0..100 {
        let mut rows = c
            .query(
                "SELECT id FROM entries WHERE fts_match(body,'token999') LIMIT 40",
                (),
            )
            .await?;
        ensure!(rows.next().await?.unwrap().get::<i64>(0)? == 999);
    }
    println!(
        "fts_selective_100_queries_ms={}",
        start.elapsed().as_millis()
    );
    c.execute_batch("BEGIN IMMEDIATE; DELETE FROM entries WHERE id IN (SELECT id FROM entries ORDER BY sequence DESC LIMIT -1 OFFSET 100); COMMIT;").await?;
    ensure!(scalar(&c, "SELECT count(*) FROM entries").await? == 100);
    ensure!(scalar(&c, "SELECT count(*) FROM payloads").await? == 0);
    checkpoint(&c).await?;
    let before = std::fs::metadata(path)?.len();
    c.execute("VACUUM", ()).await?;
    checkpoint(&c).await?;
    println!(
        "db_bytes_before_vacuum={before} after={}",
        std::fs::metadata(path)?.len()
    );
    Ok(())
}

async fn checkpoint(c: &turso::Connection) -> Result<()> {
    let mut rows = c.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
    let row = rows.next().await?.unwrap();
    ensure!(row.get::<i64>(0)? == 0, "checkpoint busy");
    while rows.next().await?.is_some() {}
    Ok(())
}

async fn crash_writer(path: &Path, commit: bool) -> Result<()> {
    let (_db, c) = open(path).await?;
    c.execute("BEGIN IMMEDIATE", ()).await?;
    c.execute("UPDATE entries SET revision=2 WHERE id=1000", ())
        .await?;
    if commit {
        c.execute("COMMIT", ()).await?;
    } else {
        c.execute("DELETE FROM entries", ()).await?;
    }
    // No destructors: exercise WAL recovery rather than orderly close.
    std::process::exit(0)
}
async fn recover(path: &Path) -> Result<()> {
    let (_db, c) = open(path).await?;
    ensure!(scalar(&c, "SELECT revision FROM entries WHERE id=1000").await? == 2);
    ensure!(scalar(&c, "SELECT count(*) FROM entries").await? == 100);
    checkpoint(&c).await?;
    let mut rows = c.query("PRAGMA integrity_check", ()).await?;
    ensure!(rows.next().await?.unwrap().get::<String>(0)? == "ok");
    Ok(())
}
