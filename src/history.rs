//! Long-term link history. Raw samples are kept 30 days; each finished hour is rolled up
//! per link end into `link_hours` (median, p10 and p90), kept 400 days. Firmware changes
//! are remembered in `fw_changes` for chart markers and to restart drift measurement.
//! These tables sit outside the projections, so a rebuild after a trust change doesn't
//! lose history the core has already pruned.

use std::collections::BTreeMap;

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::s;
use ridgewarden_client::util::{norm_node, valid_node};
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

pub const HOUR: i64 = 3_600_000;
pub const DAY: i64 = 24 * HOUR;
pub const RAW_KEEP: i64 = 30 * DAY;
const HOURS_KEEP: i64 = 400 * DAY;
/// Hours are rolled up a day after they end, so samples from a site that was cut off
/// for a while still make it in.
pub const ROLLUP_LAG: i64 = DAY;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS link_hours (
    node      TEXT NOT NULL,
    neighbor  TEXT NOT NULL,
    hour      INTEGER NOT NULL,
    n         INTEGER NOT NULL,
    snr       REAL,
    snr_p10   REAL,
    snr_p90   REAL,
    quality   REAL,
    nquality  REAL,
    noise     REAL,
    PRIMARY KEY (node, neighbor, hour)
);
CREATE TABLE IF NOT EXISTS fw_changes (
    node     TEXT NOT NULL,
    ts       INTEGER NOT NULL,
    firmware TEXT NOT NULL,
    PRIMARY KEY (node, ts)
);
CREATE TABLE IF NOT EXISTS link_notes (
    event_id TEXT PRIMARY KEY,
    ts       INTEGER NOT NULL,
    node     TEXT NOT NULL,
    other    TEXT,
    call     TEXT NOT NULL,
    text     TEXT NOT NULL,
    change   INTEGER NOT NULL
);";

/// One hour of one link end.
#[derive(Clone, Debug, PartialEq)]
pub struct Hour {
    pub hour: i64,
    /// raw samples behind it
    pub n: usize,
    pub snr: Option<f64>,
    pub snr_p10: Option<f64>,
    pub snr_p90: Option<f64>,
    pub quality: Option<f64>,
    pub nquality: Option<f64>,
    pub noise: Option<f64>,
}

/// The value at fraction `q` of the sorted values (nearest rank).
pub fn quantile(v: &mut [f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let i = ((v.len() - 1) as f64 * q).round() as usize;
    Some(v[i])
}

type RawRow = (
    String,
    String,
    i64,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
);

fn summarize(hour: i64, rows: &[&RawRow]) -> Hour {
    let col =
        |f: fn(&RawRow) -> Option<f64>| -> Vec<f64> { rows.iter().filter_map(|r| f(r)).collect() };
    let mut snr = col(|r| r.3);
    Hour {
        hour,
        n: rows.len(),
        snr: quantile(&mut snr, 0.5),
        snr_p10: quantile(&mut snr, 0.1),
        snr_p90: quantile(&mut snr, 0.9),
        quality: quantile(&mut col(|r| r.4), 0.5),
        nquality: quantile(&mut col(|r| r.5), 0.5),
        noise: quantile(&mut col(|r| r.6), 0.5),
    }
}

/// Hourly summaries of raw samples since `from`, per (node, neighbor).
pub fn hours_from_raw(
    conn: &Connection,
    from: i64,
    to: i64,
) -> Result<BTreeMap<(String, String), Vec<Hour>>> {
    let mut st = conn.prepare(
        "SELECT node, neighbor, ts - (ts % 3600000), snr, quality, nquality, noise FROM link_samples
         WHERE ts >= ?1 AND ts < ?2 ORDER BY node, neighbor, ts",
    )?;
    let rows: Vec<RawRow> = st
        .query_map(params![from, to], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut grouped: BTreeMap<(String, String, i64), Vec<&RawRow>> = BTreeMap::new();
    for r in &rows {
        grouped
            .entry((r.0.clone(), r.1.clone(), r.2))
            .or_default()
            .push(r);
    }
    let mut out: BTreeMap<(String, String), Vec<Hour>> = BTreeMap::new();
    for ((node, nb, hour), rs) in grouped {
        out.entry((node, nb))
            .or_default()
            .push(summarize(hour, &rs));
    }
    Ok(out)
}

/// Start of the first hour not yet rolled up.
fn rolled_to(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT coalesce(max(hour) + 3600000, 0) FROM link_hours",
        [],
        |r| r.get(0),
    )?)
}

/// Roll finished hours up, note firmware changes, and drop raw data past its keep time.
/// An hour is rolled again when it has more raw samples than its rollup counted, so late
/// samples from a site that was cut off still make it in; it never shrinks, since after
/// a rebuild the core may hold less of an old hour than was rolled up.
pub fn rollup(conn: &Connection, now: i64) -> Result<()> {
    let edge = now - now.rem_euclid(HOUR) - ROLLUP_LAG;
    // never rolled up: roll it, at any age. Rolled before: roll again only if it has gained
    // results, and not the oldest raw hour, which may be partly pruned already
    let from = now - now.rem_euclid(HOUR) - RAW_KEEP + HOUR;
    let stale: Vec<i64> = conn
        .prepare(
            "SELECT r.hour FROM
               (SELECT ts - (ts % 3600000) AS hour, count(*) AS n FROM link_samples
                WHERE ts < ?2 GROUP BY 1) r
             LEFT JOIN (SELECT hour, sum(n) AS n FROM link_hours GROUP BY hour) h
               ON h.hour = r.hour
             WHERE h.n IS NULL OR (r.n > h.n AND r.hour >= ?1)",
        )?
        .query_map(params![from, edge], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let tx = conn.unchecked_transaction()?;
    for hour in stale {
        tx.execute("DELETE FROM link_hours WHERE hour = ?1", params![hour])?;
        for ((node, nb), hours) in hours_from_raw(conn, hour, hour + HOUR)? {
            for h in hours {
                tx.execute(
                    "INSERT INTO link_hours
                     (node, neighbor, hour, n, snr, snr_p10, snr_p90, quality, nquality, noise)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        node, nb, h.hour, h.n as i64, h.snr, h.snr_p10, h.snr_p90, h.quality,
                        h.nquality, h.noise
                    ],
                )?;
            }
        }
    }
    tx.commit()?;
    note_firmware(conn)?;
    conn.execute(
        "DELETE FROM link_samples WHERE ts < ?1",
        params![now - RAW_KEEP],
    )?;
    conn.execute("DELETE FROM polls WHERE ts < ?1", params![now - RAW_KEEP])?;
    conn.execute(
        "DELETE FROM link_hours WHERE hour < ?1",
        params![now - HOURS_KEEP],
    )?;
    conn.execute(
        "DELETE FROM fw_changes WHERE ts < ?1",
        params![now - HOURS_KEEP],
    )?;
    Ok(())
}

/// Record each poll where a node's firmware differs from its previous poll.
fn note_firmware(conn: &Connection) -> Result<()> {
    let mut st = conn.prepare(
        "SELECT node, ts, firmware FROM polls WHERE ok = 1 AND firmware IS NOT NULL ORDER BY node, ts",
    )?;
    let rows: Vec<(String, i64, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut prev: Option<(&str, &str)> = None;
    for (node, ts, fw) in &rows {
        if let Some((pn, pf)) = prev
            && pn == node
            && pf != fw
        {
            conn.execute(
                "INSERT OR IGNORE INTO fw_changes (node, ts, firmware) VALUES (?1, ?2, ?3)",
                params![node, ts, fw],
            )?;
        }
        prev = Some((node, fw));
    }
    Ok(())
}

/// Hourly history of one link end since `from`: rolled-up hours, then raw samples
/// summarized on the fly for the hours not rolled up yet.
pub fn hours(conn: &Connection, node: &str, nb: &str, from: i64, now: i64) -> Result<Vec<Hour>> {
    let mut st = conn.prepare(
        "SELECT hour, n, snr, snr_p10, snr_p90, quality, nquality, noise FROM link_hours
         WHERE node = ?1 AND neighbor = ?2 AND hour >= ?3 ORDER BY hour",
    )?;
    let mut out: Vec<Hour> = st
        .query_map(params![node, nb, from], |r| {
            Ok(Hour {
                hour: r.get(0)?,
                n: r.get::<_, i64>(1)? as usize,
                snr: r.get(2)?,
                snr_p10: r.get(3)?,
                snr_p90: r.get(4)?,
                quality: r.get(5)?,
                nquality: r.get(6)?,
                noise: r.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let raw_from = rolled_to(conn)?.max(from);
    let mut raw = hours_from_raw(conn, raw_from, now + 1)?;
    out.extend(
        raw.remove(&(node.to_string(), nb.to_string()))
            .unwrap_or_default(),
    );
    Ok(out)
}

/// Every link end's hourly history since `from`, for analysis.
pub fn all_hours(
    conn: &Connection,
    from: i64,
    now: i64,
) -> Result<BTreeMap<(String, String), Vec<Hour>>> {
    let mut st = conn.prepare(
        "SELECT node, neighbor, hour, n, snr, snr_p10, snr_p90, quality, nquality, noise FROM link_hours
         WHERE hour >= ?1 ORDER BY node, neighbor, hour",
    )?;
    let mut out: BTreeMap<(String, String), Vec<Hour>> = BTreeMap::new();
    for r in st.query_map(params![from], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            Hour {
                hour: r.get(2)?,
                n: r.get::<_, i64>(3)? as usize,
                snr: r.get(4)?,
                snr_p10: r.get(5)?,
                snr_p90: r.get(6)?,
                quality: r.get(7)?,
                nquality: r.get(8)?,
                noise: r.get(9)?,
            },
        ))
    })? {
        let (k, h) = r?;
        out.entry(k).or_default().push(h);
    }
    for (k, hs) in hours_from_raw(conn, rolled_to(conn)?.max(from), now + 1)? {
        out.entry(k).or_default().extend(hs);
    }
    Ok(out)
}

/// Something worth marking on a chart: a firmware change or a note.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub ts: i64,
    pub node: String,
    pub text: String,
    /// a change at the radio (firmware, realignment, new hardware): drift is measured
    /// from the latest one
    pub change: bool,
}

/// Firmware changes and notes for either end of a link, oldest first.
pub fn marks(conn: &Connection, a: &str, b: &str) -> Result<Vec<Mark>> {
    let mut out = vec![];
    let mut st =
        conn.prepare("SELECT ts, node, firmware FROM fw_changes WHERE node IN (?1, ?2)")?;
    for r in st.query_map(params![a, b], |r| {
        Ok((r.get(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })? {
        let (ts, node, fw) = r?;
        out.push(Mark {
            ts,
            text: format!("{node} firmware {fw}"),
            node,
            change: true,
        });
    }
    let mut st = conn.prepare(
        "SELECT ts, node, call, text, change FROM link_notes
         WHERE (node = ?1 AND (other IS NULL OR other = ?2)) OR (node = ?2 AND (other IS NULL OR other = ?1))",
    )?;
    for r in st.query_map(params![a, b], |r| {
        Ok((
            r.get(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get(4)?,
        ))
    })? {
        let (ts, node, call, text, change) = r?;
        out.push(Mark {
            ts,
            text: format!("{call}: {text}"),
            node,
            change,
        });
    }
    out.sort_by_key(|m| m.ts);
    Ok(out)
}

/// `link.note` events: a ham's note about a site or a link ("realigned the dish"),
/// shown on its charts. Notes marked as a change restart drift measurement. Its schema
/// also creates the history tables, so any app that runs link analysis has them; only
/// the notes are cleared on a rebuild.
pub struct Notes;

impl Projection for Notes {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM link_notes", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "link.note" {
            return Ok(());
        }
        let p = &e.payload;
        let node = norm_node(s(p, "node").unwrap_or(""));
        let other = s(p, "other").map(norm_node);
        let text: String = s(p, "text").unwrap_or("").chars().take(500).collect();
        if !valid_node(&node)
            || other.as_deref().is_some_and(|o| !valid_node(o))
            || text.trim().is_empty()
        {
            return Ok(());
        }
        tx.execute(
            "INSERT OR IGNORE INTO link_notes (event_id, ts, node, other, call, text, change)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.id,
                ts,
                node,
                other,
                e.author.call,
                text,
                p.get("change").and_then(Value::as_bool).unwrap_or(false)
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_pick_by_rank() {
        let mut v: Vec<f64> = (1..=11).map(f64::from).collect();
        assert_eq!(quantile(&mut v, 0.5), Some(6.0));
        assert_eq!(quantile(&mut v, 0.1), Some(2.0));
        assert_eq!(quantile(&mut v, 0.9), Some(10.0));
        assert_eq!(quantile(&mut [], 0.5), None);
    }
}
