//! The track record, so the spec's success measures can be read off the app. Each
//! analysis round notes every finding (an alert on a link or at a site); one unseen for
//! `GAP` starts a new episode when it comes back. From that: how many lost links had a
//! warning first, and whether the problems at a site cleared after a visit was noted.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::history::{DAY, HOUR};
use crate::links::AlertKind;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS findings (
    a        TEXT NOT NULL,
    b        TEXT NOT NULL,
    kind     TEXT NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms  INTEGER NOT NULL,
    PRIMARY KEY (a, b, kind, first_ms)
);
CREATE TABLE IF NOT EXISTS aim_steps (
    a      TEXT NOT NULL,
    b      TEXT NOT NULL,
    before REAL NOT NULL,
    at     INTEGER NOT NULL,
    PRIMARY KEY (a, b)
);";

/// long enough that restarting the app or the box doesn't split one problem in two
const GAP: i64 = 6 * HOUR;
/// a fix only counts once the problem has stayed away this long
const STAYED: i64 = 7 * DAY;
const KEEP: i64 = 400 * DAY;
/// a warning counts for a later loss if it was showing this long before the loss
const AHEAD: i64 = 30 * DAY;

/// Remember the level before each step called aim, so its alert can stay open after the
/// step is too old for the analysis to see, until the link is back near that level.
pub fn note_aims(conn: &Connection, steps: &[(String, String, f64, i64)]) -> Result<()> {
    for (a, b, before, at) in steps {
        conn.execute(
            "INSERT OR REPLACE INTO aim_steps (a, b, before, at) VALUES (?1, ?2, ?3, ?4)",
            params![a, b, before, at],
        )?;
    }
    Ok(())
}

/// (a, b, level before, when) for every step remembered.
pub fn aim_steps(conn: &Connection) -> Result<Vec<(String, String, f64, i64)>> {
    let mut st = conn.prepare("SELECT a, b, before, at FROM aim_steps")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn forget_aim(conn: &Connection, a: &str, b: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM aim_steps WHERE a = ?1 AND b = ?2",
        params![a, b],
    )?;
    Ok(())
}

/// One thing found: on the a-b link, or at site `a` when `b` is empty.
pub type Finding = (String, String, AlertKind);

pub fn note(conn: &Connection, found: &[Finding], now: i64) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for (a, b, kind) in found {
        let n = tx.execute(
            "UPDATE findings SET last_ms = ?4 WHERE a = ?1 AND b = ?2 AND kind = ?3 AND last_ms >= ?5",
            params![a, b, kind.name(), now, now - GAP],
        )?;
        if n == 0 {
            tx.execute(
                "INSERT OR IGNORE INTO findings (a, b, kind, first_ms, last_ms) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![a, b, kind.name(), now],
            )?;
        }
    }
    tx.execute(
        "DELETE FROM findings WHERE last_ms < ?1",
        params![now - KEEP],
    )?;
    tx.commit()?;
    Ok(())
}

/// A link that was lost, and the earliest warning about it that was showing in the
/// month before.
#[derive(Clone, Debug, PartialEq)]
pub struct Loss {
    pub a: String,
    pub b: String,
    pub at: i64,
    pub warning: Option<(AlertKind, i64)>,
}

pub fn losses(conn: &Connection) -> Result<Vec<Loss>> {
    let mut st = conn.prepare(
        "SELECT a, b, first_ms FROM findings WHERE kind = 'loss' ORDER BY first_ms DESC LIMIT 200",
    )?;
    let lost: Vec<(String, String, i64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut out = vec![];
    for (a, b, at) in lost {
        let warning: Option<(String, i64)> = conn
            .query_row(
                "SELECT kind, first_ms FROM findings WHERE a = ?1 AND b = ?2
                 AND kind NOT IN ('loss', 'below-plan') AND first_ms < ?3 AND last_ms >= ?4 ORDER BY first_ms LIMIT 1",
                params![a, b, at, at - AHEAD],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        out.push(Loss {
            warning: warning.and_then(|(k, t)| Some((AlertKind::from_name(&k)?, t))),
            a,
            b,
            at,
        });
    }
    Ok(out)
}

/// A noted visit to a site, with what was wrong there at the time.
#[derive(Clone, Debug, PartialEq)]
pub struct Visit {
    pub ts: i64,
    pub node: String,
    pub call: String,
    pub text: String,
    /// findings at the site or on its links when the note was made
    pub open: usize,
    /// of those, how many were gone within a day and stayed gone for a week
    pub cleared: usize,
    /// the note is less than a week old, so `cleared` can't be judged yet
    pub pending: bool,
}

pub fn visits(conn: &Connection, now: i64) -> Result<Vec<Visit>> {
    let mut st = conn.prepare(
        "SELECT ts, node, call, text, other FROM link_notes WHERE ts > ?1 ORDER BY ts DESC LIMIT 100",
    )?;
    let notes: Vec<(i64, String, String, String, Option<String>)> = st
        .query_map([now - KEEP], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut st = conn.prepare(
        "SELECT a, b, kind, last_ms FROM findings WHERE (a = ?1 OR b = ?1 OR a = ?2 OR b = ?2)
         AND first_ms <= ?3 AND last_ms >= ?3 - ?4",
    )?;
    let mut out = vec![];
    for (ts, node, call, text, other) in notes {
        let other = other.unwrap_or_else(|| node.clone());
        let open: Vec<(String, String, String, i64)> = st
            .query_map(params![node, other, ts, GAP], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut cleared = 0;
        for (a, b, kind, last) in &open {
            let back: bool = conn.query_row(
                "SELECT count(*) > 0 FROM findings WHERE a = ?1 AND b = ?2 AND kind = ?3
                 AND first_ms > ?4 AND first_ms < ?5",
                params![a, b, kind, last, ts + STAYED],
                |r| r.get(0),
            )?;
            if *last < ts + DAY && *last < now - GAP && !back {
                cleared += 1;
            }
        }
        out.push(Visit {
            pending: now < ts + STAYED,
            ts,
            node,
            call,
            text,
            open: open.len(),
            cleared,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Notes;
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    const T: i64 = 1_800_000_000_000;

    fn found(a: &str, b: &str, k: AlertKind) -> Finding {
        (a.into(), b.into(), k)
    }

    fn db() -> Fake {
        let f = Fake::new(vec![Box::new(Notes)]);
        f.conn.execute_batch(SCHEMA).unwrap();
        f
    }

    #[test]
    fn a_loss_after_a_warning_counts_as_caught() {
        let f = db();
        let c = &f.conn;
        // drift on a-b for two days, then a gap, then the link is lost a week later
        for h in 0..48 {
            note(c, &[found("a", "b", AlertKind::Drift)], T + h * HOUR).unwrap();
        }
        // falling short of the planner is no warning of a failure
        note(c, &[found("a", "c", AlertKind::BelowPlan)], T).unwrap();
        note(c, &[found("a", "c", AlertKind::Loss)], T + 3 * DAY).unwrap();
        note(c, &[found("a", "b", AlertKind::Loss)], T + 9 * DAY).unwrap();
        let l = losses(c).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].warning, Some((AlertKind::Drift, T)));
        assert_eq!(l[1].warning, None, "a-c gave no warning");
    }

    #[test]
    fn episodes_split_after_a_gap() {
        let f = db();
        let c = &f.conn;
        note(c, &[found("a", "b", AlertKind::Flap)], T).unwrap();
        note(c, &[found("a", "b", AlertKind::Flap)], T + HOUR).unwrap();
        note(c, &[found("a", "b", AlertKind::Flap)], T + 8 * HOUR).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM findings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn a_visit_that_fixed_things_shows() {
        let mut f = db();
        for h in 0..30 {
            let mut now = vec![found("tst-x", "tst-y", AlertKind::OneWay)];
            if h < 25 {
                now.push(found("tst-x", "", AlertKind::Noise));
            }
            note(&f.conn, &now, T + h * HOUR).unwrap();
        }
        f.add(
            "link.note",
            json!({"node": "tst-x", "text": "new feed line"}),
            T + 24 * HOUR,
        );
        let v = visits(&f.conn, T + 3 * DAY).unwrap();
        assert!(v[0].pending, "too soon to say it stayed fixed");
        let v = visits(&f.conn, T + 9 * DAY).unwrap();
        assert_eq!((v[0].open, v[0].cleared, v[0].pending), (2, 2, false));
        // the one-way problem comes back two days later: not fixed after all
        note(
            &f.conn,
            &[found("tst-x", "tst-y", AlertKind::OneWay)],
            T + 3 * DAY,
        )
        .unwrap();
        let v = visits(&f.conn, T + 9 * DAY).unwrap();
        assert_eq!((v[0].open, v[0].cleared), (2, 1));
    }
}
