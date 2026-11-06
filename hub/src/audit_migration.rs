//! Transaction-scoped upgrade of the legacy event table; no inferred legacy history.
use rusqlite::Connection;

// Both staging and final tables use the same owned definition. Recreate the final
// name directly: ALTER RENAME validates views/triggers while events is absent.
const EVENT_COLUMNS: &str = "
    id INTEGER PRIMARY KEY, task_id TEXT NOT NULL, actor TEXT NOT NULL,
    action TEXT NOT NULL, at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    from_status TEXT, to_status TEXT, task_revision INTEGER, revision INTEGER,
    delivery_seq INTEGER, FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE RESTRICT";

pub(super) fn upgrade(db: &Connection) -> rusqlite::Result<()> {
    // Validate before rebuilding, even on connections where foreign_keys is OFF.
    let orphans: i64 = db.query_row(
        "SELECT count(*) FROM events e WHERE NOT EXISTS (SELECT 1 FROM tasks t WHERE t.id=e.task_id)",
        [], |r| r.get(0))?;
    if orphans != 0 {
        return Err(rusqlite::Error::InvalidParameterName(
            "orphan audit events: reconcile task references before upgrade".into(),
        ));
    }
    // Version-zero adoption may encounter the already-upgraded valid layout.
    // Structural validation owns the FK/column contract; never reinterpret links.
    let upgraded: i64 = db.query_row(
        "SELECT count(*) FROM pragma_table_info('events') WHERE name IN ('revision','delivery_seq')",
        [], |r|r.get(0))?;
    if upgraded == 2 {
        return Ok(());
    }
    // DROP TABLE performs implicit DELETE for FK purposes and can CASCADE into
    // unrelated tables. Never depend on the connection's default FK setting.
    // Scan every main-schema table, not only Hub-owned tables. Block even when
    // enforcement is OFF, so behavior is identical across SQLite builds.
    let incoming: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_schema s JOIN pragma_foreign_key_list(s.name) f
         WHERE s.type='table' AND f.[table]='events' COLLATE NOCASE",
        [],
        |row| row.get(0),
    )?;
    if incoming != 0 {
        return Err(rusqlite::Error::InvalidParameterName(
            "incoming foreign keys reference events; dependent tables require an explicit coordinated migration".into()));
    }
    // v6 captured revision but not the delivery link. Reconcile only an exact,
    // unique, immutable submitted version, never a private draft or legacy guess.
    let unlinked: i64 = db.query_row(
        "SELECT count(*) FROM events e WHERE e.action='deliver' AND e.task_revision IS NOT NULL
         AND (SELECT count(*) FROM deliveries d WHERE d.task_id=e.task_id
           AND d.status='submitted' AND d.task_revision=e.task_revision AND d.author=e.actor)!=1",
        [],
        |r| r.get(0),
    )?;
    if unlinked != 0 {
        return Err(rusqlite::Error::InvalidParameterName(
            "recorded delivery audit link is missing or ambiguous; reconcile before upgrade".into(),
        ));
    }
    // Preserve application/user indexes and triggers, including append-only guards.
    // A recreation error rolls the enclosing migration transaction back.
    let mut query = db.prepare("SELECT sql FROM sqlite_schema WHERE tbl_name='events' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name")?;
    let objects = query
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(query);
    db.execute_batch(&format!("CREATE TABLE events_upgrade_v8 ({EVENT_COLUMNS})"))?;
    db.execute_batch("INSERT INTO events_upgrade_v8(id,task_id,actor,action,at,from_status,to_status,task_revision,revision,delivery_seq)
        SELECT e.id,e.task_id,e.actor,e.action,e.at,e.from_status,e.to_status,e.task_revision,e.task_revision,
          CASE WHEN e.action='deliver' AND e.task_revision IS NOT NULL THEN
            (SELECT d.seq FROM deliveries d WHERE d.task_id=e.task_id AND d.status='submitted'
             AND d.task_revision=e.task_revision AND d.author=e.actor) ELSE NULL END
        FROM events e;
        DROP TABLE events;")?;
    // Views and cross-table triggers remain in place with their SQL unchanged.
    // Copy before restoring event-owned triggers, so no trigger bodies execute.
    db.execute_batch(&format!("CREATE TABLE events ({EVENT_COLUMNS})"))?;
    db.execute_batch(
        "INSERT INTO events SELECT * FROM events_upgrade_v8;
        DROP TABLE events_upgrade_v8;",
    )?;
    for sql in objects {
        db.execute_batch(&sql)?;
    }
    let violations: i64 = db.query_row(
        "SELECT count(*) FROM pragma_foreign_key_check('events')",
        [],
        |r| r.get(0),
    )?;
    if violations != 0 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn old_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY);
          INSERT INTO tasks VALUES ('task');
          CREATE TABLE events(id INTEGER PRIMARY KEY,task_id TEXT NOT NULL,actor TEXT NOT NULL,action TEXT NOT NULL,
          at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,from_status TEXT,to_status TEXT,task_revision INTEGER);
          CREATE TABLE deliveries(task_id TEXT,seq INTEGER,author TEXT,status TEXT,task_revision INTEGER);
          CREATE INDEX retained_audit_index ON events(task_id,task_revision);
          CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT,'immutable'); END;").unwrap();
        db
    }
    #[test]
    fn preserves_legacy_and_reconciles_only_unique_submitted_link() {
        let mut db = old_db();
        db.execute_batch("INSERT INTO events(id,task_id,actor,action,at) VALUES (7,'task','alice','created','old-time');
          INSERT INTO events(id,task_id,actor,action,from_status,to_status,task_revision) VALUES (9,'task','bob','deliver','working','ready_for_acceptance',2);
          INSERT INTO deliveries VALUES ('task',31,'bob','draft',2),('task',1,'bob','submitted',2);").unwrap();
        {
            let tx = db.transaction().unwrap();
            upgrade(&tx).unwrap();
            tx.commit().unwrap();
        }
        let legacy: (String, Option<i64>, Option<i64>) = db
            .query_row(
                "SELECT at,revision,delivery_seq FROM events WHERE id=7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(legacy, ("old-time".into(), None, None));
        assert_eq!(
            db.query_row("SELECT delivery_seq FROM events WHERE id=9", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(db
            .execute_batch("UPDATE events SET action='cancel'")
            .is_err());
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='retained_audit_index'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        assert!(db
            .execute_batch(
                "INSERT INTO events(task_id,actor,action) VALUES ('missing','alice','created')"
            )
            .is_err());
        assert!(db
            .execute_batch("DELETE FROM tasks WHERE id='task'")
            .is_err());
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM pragma_foreign_key_list('events')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
    #[test]
    fn orphan_upgrade_rolls_back_without_deleting_or_rewriting_legacy() {
        let mut db = old_db();
        db.execute_batch("INSERT INTO events(task_id,actor,action) VALUES ('missing','alice','created'); PRAGMA user_version=7;").unwrap();
        {
            let tx = db.transaction().unwrap();
            assert!(upgrade(&tx).unwrap_err().to_string().contains("orphan"));
        }
        assert_eq!(
            db.query_row("SELECT task_id FROM events", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "missing"
        );
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM pragma_table_info('events') WHERE name='delivery_seq'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
    #[test]
    fn ambiguous_or_missing_recorded_link_fails_without_guessing() {
        for count in [0, 2] {
            let mut db = old_db();
            db.execute_batch("INSERT INTO events(task_id,actor,action,task_revision) VALUES ('task','bob','deliver',2)").unwrap();
            for seq in 1..=count {
                db.execute(
                    "INSERT INTO deliveries VALUES ('task',?1,'bob','submitted',2)",
                    [seq],
                )
                .unwrap();
            }
            {
                let tx = db.transaction().unwrap();
                assert!(upgrade(&tx).unwrap_err().to_string().contains("ambiguous"));
            }
            assert_eq!(
                db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }
    #[test]
    fn late_failure_rolls_back_rebuilt_table_and_preserves_data() {
        let mut db = old_db();
        db.execute_batch(
            "INSERT INTO events(task_id,actor,action) VALUES ('task','alice','created')",
        )
        .unwrap();
        {
            let tx = db.transaction().unwrap();
            upgrade(&tx).unwrap(); /* emulate later migration failure */
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM pragma_table_info('events') WHERE name='delivery_seq'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM pragma_foreign_key_list('events')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
    #[test]
    fn views_and_cross_table_triggers_survive_without_execution_or_rewrite() {
        for foreign_keys in [false, true] {
            let mut db = old_db();
            db.pragma_update(None, "foreign_keys", foreign_keys)
                .unwrap();
            db.execute_batch("INSERT INTO events(id,task_id,actor,action) VALUES (7,'task','alice','created');
                CREATE VIEW retained_view AS SELECT id,action FROM events;
                CREATE VIEW retained_nested_view AS SELECT * FROM retained_view;
                CREATE TABLE cross_trigger_source(id INTEGER);
                CREATE TRIGGER retained_cross_trigger AFTER INSERT ON cross_trigger_source
                BEGIN INSERT INTO events(id,task_id,actor,action) VALUES (NEW.id,'task','bob','accept'); END;").unwrap();
            let objects = |db: &Connection| -> Vec<(String, String)> {
                db.prepare("SELECT name,sql FROM sqlite_schema WHERE name IN ('retained_view','retained_nested_view','retained_cross_trigger') ORDER BY name")
                    .unwrap().query_map([], |r|Ok((r.get(0)?,r.get(1)?))).unwrap()
                    .collect::<rusqlite::Result<_>>().unwrap()
            };
            let before = objects(&db);
            {
                let tx = db.transaction().unwrap();
                upgrade(&tx).unwrap();
                tx.commit().unwrap();
            }
            assert_eq!(objects(&db), before);
            assert_eq!(
                db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM cross_trigger_source", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            assert_eq!(
                db.query_row(
                    "SELECT action FROM retained_nested_view WHERE id=7",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "created"
            );
            db.execute_batch("INSERT INTO cross_trigger_source VALUES (8)")
                .unwrap();
            assert_eq!(
                db.query_row("SELECT action FROM retained_view WHERE id=8", [], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap(),
                "accept"
            );
        }
    }

    #[test]
    fn incoming_foreign_keys_fail_before_drop_even_when_enforcement_is_off() {
        for foreign_keys in [false, true] {
            for rule in ["CASCADE", "SET NULL", "RESTRICT", "NO ACTION"] {
                let mut db = old_db();
                db.pragma_update(None, "foreign_keys", foreign_keys)
                    .unwrap();
                db.execute_batch("INSERT INTO events(id,task_id,actor,action) VALUES (7,'task','alice','created')").unwrap();
                db.execute_batch(&format!("CREATE TABLE external_notes(id INTEGER PRIMARY KEY,event_id INTEGER REFERENCES Events(id) ON DELETE {rule},note TEXT);
                    INSERT INTO external_notes VALUES (1,7,'must survive');")).unwrap();
                let before: String = db
                    .query_row(
                        "SELECT sql FROM sqlite_schema WHERE name='events'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                {
                    let tx = db.transaction().unwrap();
                    assert!(upgrade(&tx)
                        .unwrap_err()
                        .to_string()
                        .contains("incoming foreign keys"));
                    // Check while the failed transaction is still open: no cascade occurred.
                    assert_eq!(
                        tx.query_row("SELECT event_id FROM external_notes", [], |r| r
                            .get::<_, i64>(0))
                            .unwrap(),
                        7
                    );
                }
                assert_eq!(
                    db.query_row(
                        "SELECT sql FROM sqlite_schema WHERE name='events'",
                        [],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                    before
                );
                assert_eq!(
                    db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    1
                );
                assert_eq!(
                    db.query_row("SELECT note FROM external_notes", [], |r| r
                        .get::<_, String>(0))
                        .unwrap(),
                    "must survive"
                );
                assert_eq!(
                    db.query_row(
                        "SELECT count(*) FROM sqlite_schema WHERE name='events_upgrade_v8'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                    0
                );
            }
        }
    }
}
