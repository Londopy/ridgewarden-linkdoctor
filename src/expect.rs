//! What the planner (spec 03) expects each link to measure, so a link can be judged
//! against its path and not only against its own past: "should be 25 dB, measures 18".
//!
//! The planner doesn't publish predictions yet. This reads the event it would publish,
//! `planner.prediction`, in the main log since a prediction should outlast telemetry:
//! `{"a": "tst-cuesta", "b": "tst-madonna", "snr_db": 25.0, "model": "ITM",
//! "calibrated": true}`, with `model` and `calibrated` optional. The newest prediction for a link replaces older ones.

use std::collections::BTreeMap;

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::graph::{Edge, edge};
use ridgewarden_client::mesh::{f, s};
use ridgewarden_client::util::{norm_node, valid_node};
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

use crate::config::LinksCfg;
use crate::links::{Alert, AlertKind, LinkState, PairView};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS predictions (
    a          TEXT NOT NULL,
    b          TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    snr        REAL NOT NULL,
    model      TEXT NOT NULL,
    calibrated INTEGER NOT NULL,
    PRIMARY KEY (a, b)
);";

/// A prediction for one link.
#[derive(Clone, Debug, PartialEq)]
pub struct Expect {
    pub ts: i64,
    pub snr: f64,
    pub model: String,
    /// corrected with what this link (or links like it) actually measured
    pub calibrated: bool,
}

impl Expect {
    /// "the planner's calibrated ITM prediction"
    pub fn source(&self) -> String {
        format!(
            "the planner's {}{} prediction",
            if self.calibrated { "calibrated " } else { "" },
            self.model
        )
    }
}

pub struct Predictions;

impl Projection for Predictions {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM predictions", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "planner.prediction" {
            return Ok(());
        }
        let p = &e.payload;
        let (a, b) = edge(
            &norm_node(s(p, "a").unwrap_or("")),
            &norm_node(s(p, "b").unwrap_or("")),
        );
        let snr = f(p, "snr_db").filter(|x| (-20.0..=80.0).contains(x));
        let model: String = s(p, "model")
            .unwrap_or("path")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || " -.".contains(*c))
            .take(30)
            .collect();
        let (Some(snr), true) = (snr, valid_node(&a) && valid_node(&b) && a != b) else {
            return Ok(());
        };
        tx.execute(
            "INSERT INTO predictions (a, b, ts, snr, model, calibrated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(a, b) DO UPDATE SET ts = excluded.ts, snr = excluded.snr,
                 model = excluded.model, calibrated = excluded.calibrated
             WHERE excluded.ts >= predictions.ts",
            params![
                a,
                b,
                ts,
                snr,
                if model.trim().is_empty() {
                    "path".into()
                } else {
                    model
                },
                p.get("calibrated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            ],
        )?;
        Ok(())
    }
}

/// What a link measures now, for comparing with a prediction: the weaker end's median
/// over the last few hours, or its latest reading.
pub fn measured(p: &PairView) -> Option<f64> {
    p.sides
        .iter()
        .filter_map(|s| s.recent_snr)
        .reduce(f64::min)
        .or_else(|| p.snr())
}

/// A link that is up but measures well short of its prediction.
pub fn shortfall(p: &PairView, e: &Expect, cfg: &LinksCfg) -> Option<Alert> {
    let m = measured(p)?;
    let short = e.snr - m;
    if p.state != LinkState::Up || p.link_type != "RF" || short < cfg.below_plan_db {
        return None;
    }
    Some(Alert {
        kind: AlertKind::BelowPlan,
        severity: 1.0 + (short / 20.0).min(0.5),
        text: format!(
            "Should be {:.0} dB by {}, measures {m:.0} ({short:.0} dB short). Check alignment, cables and connectors, or for something in the path the planner doesn't know about.",
            e.snr,
            e.source()
        ),
        since: None,
    })
}

/// Every link's latest prediction.
pub fn all(conn: &Connection) -> Result<BTreeMap<Edge, Expect>> {
    let mut st = conn.prepare("SELECT a, b, ts, snr, model, calibrated FROM predictions")?;
    let rows = st.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            Expect {
                ts: r.get(2)?,
                snr: r.get(3)?,
                model: r.get(4)?,
                calibrated: r.get(5)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    #[test]
    fn keeps_the_newest_valid_prediction_per_link() {
        let mut l = Fake::new(vec![Box::new(Predictions)]);
        l.add(
            "planner.prediction",
            json!({"a": "TST-B", "b": "tst-a", "snr_db": 25.0, "model": "ITM", "calibrated": true}),
            2000,
        );
        l.add(
            "planner.prediction",
            json!({"a": "tst-a", "b": "tst-b", "snr_db": 30.0}),
            1000,
        );
        l.add(
            "planner.prediction",
            json!({"a": "tst-a", "b": "tst-c", "snr_db": 400.0}),
            3000,
        );
        l.add(
            "planner.prediction",
            json!({"a": "tst-a", "b": "tst-a", "snr_db": 20.0}),
            3000,
        );
        let p = all(&l.conn).unwrap();
        assert_eq!(p.len(), 1);
        let e = &p[&edge("tst-a", "tst-b")];
        assert_eq!(e.snr, 25.0, "an older prediction arriving late doesn't win");
        assert_eq!(e.source(), "the planner's calibrated ITM prediction");
    }

    #[test]
    fn a_link_well_short_of_its_prediction_is_flagged() {
        let mut l = Fake::new(vec![
            Box::new(ridgewarden_client::mesh::Mesh),
            Box::new(crate::history::Notes),
            Box::new(Predictions),
        ]);
        let now = 1_800_000_000_000;
        for i in 0..8 {
            l.add(
                "node.poll",
                json!({"node": "tst-a", "ok": true, "links": [
                    {"neighbor": "tst-b", "type": "RF", "snr": 14.0 + (i % 2) as f64, "quality": 95.0}
                ]}),
                now - (8 - i) * 5 * 60_000,
            );
        }
        let pairs = crate::links::analyze(&l.conn, now, 20 * 60_000, &LinksCfg::default()).unwrap();
        let e = |snr| Expect {
            ts: now,
            snr,
            model: "ITM".into(),
            calibrated: false,
        };
        let a = shortfall(&pairs[0], &e(26.0), &LinksCfg::default()).unwrap();
        assert!(
            a.text
                .starts_with("Should be 26 dB by the planner's ITM prediction, measures 14"),
            "{}",
            a.text
        );
        assert!(shortfall(&pairs[0], &e(20.0), &LinksCfg::default()).is_none());
    }
}
