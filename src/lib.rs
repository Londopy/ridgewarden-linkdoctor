//! ridgewarden-linkdoctor: long-term link history for AREDN meshes, alerts when a link
//! slowly degrades or flaps, the single points of failure, and a ranked list of what to
//! fix on the next site visit, all in plain words.
//!
//! Runs next to a ridgewarden core and builds its view from the core's event feed. Every
//! site's polls end up in every core, so any Link Doctor sees the whole mesh that the
//! cores (and the crawl, if one is on) can see.

pub mod config;
pub mod expect;
pub mod history;
pub mod links;
pub mod record;
pub mod upgrade;
pub mod weather;
pub mod web;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use ridgewarden_client::alerts::{self, Alert, Severity};
use ridgewarden_client::core::Projection;
use ridgewarden_client::graph::Edge;
use ridgewarden_client::mesh::Mesh;
use ridgewarden_client::util::now_ms;
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::Connection;

use crate::config::Config;
use crate::expect::Predictions;
use crate::history::Notes;
use crate::links::{AlertKind, LinkState};

pub const PROJECTIONS: &[&dyn Projection] = &[&Mesh, &Notes, &Predictions, &weather::Candidates];

const VIEW_TTL: Duration = Duration::from_secs(30);

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    /// the last analysis, reused for a short while: every page and the alert round need it
    view: Mutex<Option<(Instant, Arc<web::View>)>>,
    /// this site's node, once the core knows it
    here: Mutex<Option<String>>,
    /// for alerts and the weather, which may go out to the internet over TLS
    pub http: reqwest::Client,
    pub wx: weather::Wx,
}

impl App {
    pub fn new(cfg: Config, with_token: bool) -> Result<Self> {
        let token = with_token.then_some(cfg.token_file.as_path());
        let core = CoreClient::new(&cfg.core_url, token)?;
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        db.execute_batch(alerts::SCHEMA)?;
        db.execute_batch(weather::SCHEMA)?;
        db.execute_batch(record::SCHEMA)?;
        Ok(Self {
            cfg,
            core,
            db: Mutex::new(db),
            view: Mutex::new(None),
            here: Mutex::new(None),
            wx: weather::Wx::default(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(concat!(
                    "ridgewarden-linkdoctor/",
                    env!("CARGO_PKG_VERSION")
                ))
                .build()?,
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This site's node name, or "" before the core has polled it.
    pub async fn here(&self) -> String {
        if let Some(h) = self.here.lock().unwrap().clone() {
            return h;
        }
        let node = self.core.whoami().await.map(|w| w.node).unwrap_or_default();
        if !node.is_empty() {
            *self.here.lock().unwrap() = Some(node.clone());
        }
        node
    }

    /// The analysis of the whole mesh, at most `VIEW_TTL` old.
    pub fn view(&self, here: &str) -> Result<Arc<web::View>> {
        let mut cache = self.view.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = cache.as_ref()
            && at.elapsed() < VIEW_TTL
            && v.here == here
        {
            return Ok(v.clone());
        }
        let v = Arc::new(web::analyze(self, here)?);
        *cache = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    /// Drop the cached analysis, after something it depends on changed.
    pub fn invalidate(&self) {
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn fresh_ms(&self) -> i64 {
        self.cfg.fresh_s as i64 * 1000
    }
}

/// What's worth telling someone about right now. Drift and aim alerts the weather
/// holds back are left out; links below the planner's prediction go on the workday
/// list, not out as alerts.
pub fn active_alerts(app: &App, v: &web::View) -> Vec<Alert> {
    let l = |n: &str| app.cfg.label(n);
    let mut out = vec![];
    for p in &v.pairs {
        let cuts_off =
            p.state == LinkState::Down && !links::restored_by(&v.topo, &p.a, &p.b).is_empty();
        let check = v.checks.get(&p.edge());
        for a in &p.alerts {
            let severity = match a.kind {
                AlertKind::BelowPlan => continue,
                AlertKind::Loss if cuts_off => Severity::Critical,
                _ => Severity::Warning,
            };
            let mut text = a.text.clone();
            if weather::applies(a.kind)
                && let Some(c) = check
            {
                if c.held.is_some() {
                    continue;
                }
                if !c.note.is_empty() {
                    text = format!("{text} {}", c.note);
                }
            }
            out.push(Alert {
                key: alert_key(&p.a, &p.b, a.kind),
                severity,
                title: format!("{} - {}: {}", l(&p.a), l(&p.b), a.kind.word()),
                text,
            });
        }
    }
    for n in &v.nodes {
        out.push(Alert {
            key: format!("site {} {:?}", n.node, n.alert.kind),
            severity: Severity::Warning,
            title: format!("{}: {}", l(&n.node), n.alert.kind.word()),
            text: n.alert.text.clone(),
        });
    }
    out
}

fn alert_key(a: &str, b: &str, kind: AlertKind) -> String {
    format!("link {a}|{b} {kind:?}")
}

/// Whether an open alert should stay open although it isn't active this round. A lost
/// link stays open while nobody can say it is back: its reporters have gone quiet, or it
/// dropped out of the analysis (no samples for a week) while either end is still polled.
/// A drift or aim alert the weather holds back stays open, since rain fixes neither, and
/// an aim alert stays open while the link is still well below where it was before the
/// step (in `aims_off`), even once the step is too old for the analysis to see.
fn keep_open(v: &web::View, aims_off: &BTreeSet<Edge>, key: &str) -> bool {
    let Some((pair, kind)) = key.strip_prefix("link ").and_then(|k| k.rsplit_once(' ')) else {
        return false;
    };
    let Some((a, b)) = pair.split_once('|') else {
        return false;
    };
    let e = (a.to_string(), b.to_string());
    let held = || v.checks.get(&e).is_some_and(|c| c.held.is_some());
    match kind {
        "Loss" => match v.pairs.iter().find(|p| p.a == a && p.b == b) {
            Some(p) => p.state == LinkState::Unknown,
            None => v.observed.contains(a) || v.observed.contains(b),
        },
        "Drift" => held(),
        "Aim" => held() || aims_off.contains(&e),
        _ => false,
    }
}

/// Links whose aim alert should stay open: remembered steps where the link is still
/// more than half a step below its old level and nobody has noted a radio change since.
/// Steps that are over are forgotten.
fn aims_off(app: &App, v: &web::View) -> Result<BTreeSet<Edge>> {
    let db = app.db();
    let mut steps = vec![];
    for p in &v.pairs {
        if p.alerts.iter().any(|a| a.kind == AlertKind::Aim)
            && let Some(st) = p
                .sides
                .iter()
                .filter_map(|s| s.step.as_ref())
                .max_by(|x, y| x.fall().total_cmp(&y.fall()))
        {
            steps.push((p.a.clone(), p.b.clone(), st.before, st.at));
        }
    }
    record::note_aims(&db, &steps)?;
    let mut out = BTreeSet::new();
    for (a, b, before, at) in record::aim_steps(&db)? {
        let pair = v.pairs.iter().find(|p| p.a == a && p.b == b);
        let back = pair
            .and_then(expect::measured)
            .is_some_and(|m| m >= before - app.cfg.links.aim_step_db / 2.0);
        let changed = history::marks(&db, &a, &b)?
            .iter()
            .any(|m| m.change && m.ts > at);
        if pair.is_none() || back || changed {
            record::forget_aim(&db, &a, &b)?;
        } else {
            out.insert((a, b));
        }
    }
    Ok(out)
}

/// What the analysis shows now, for the track record.
fn findings(v: &web::View) -> Vec<record::Finding> {
    let links = v
        .pairs
        .iter()
        .flat_map(|p| p.alerts.iter().map(|a| (p.a.clone(), p.b.clone(), a.kind)));
    let sites = v
        .nodes
        .iter()
        .map(|n| (n.node.clone(), String::new(), n.alert.kind));
    links.chain(sites).collect()
}

async fn alert_round(app: &App) -> Result<usize> {
    let here = app.here().await;
    let v = app.view(&here)?;
    let active = active_alerts(app, &v);
    record::note(&app.db(), &findings(&v), v.now)?;
    let aims = aims_off(app, &v)?;
    let keep = |k: &str| keep_open(&v, &aims, k);
    alerts::run(
        &app.http,
        &app.db,
        &app.cfg.alerts,
        "ridgewarden-linkdoctor",
        active,
        &keep,
    )
    .await
}

/// Follow the core's feed, roll up history and send alerts on schedule, forever.
pub fn spawn_loops(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = ridgewarden_client::follow(&a.core, &a.db, PROJECTIONS).await {
                tracing::warn!("following the core: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    tokio::spawn(weather::run(app.clone()));
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            if let Err(e) = history::rollup(&a.db(), now_ms()) {
                tracing::warn!("rolling up link history: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    tokio::spawn(async move {
        // give the feed time to catch up first
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            match alert_round(&app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("{n} alert notices sent"),
                Err(e) => tracing::warn!("alerts: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{DAY, HOUR};
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    fn app(dir: &std::path::Path, weather: &str) -> App {
        let text = format!(
            "core_url = \"http://127.0.0.1:9\"
data_dir = {:?}
{weather}",
            dir.display().to_string()
        );
        App::new(toml::from_str(&text).unwrap(), false).unwrap()
    }

    /// Eight days of the a-b link losing 5 dB, through the app's own projections.
    fn drifting(app: &App) {
        let mut f = Fake::new(vec![]);
        let now = now_ms();
        let db = app.db();
        let mut t = now - 8 * DAY;
        while t <= now {
            let age = (now - t) as f64 / DAY as f64;
            let e = f.add(
                "node.poll",
                json!({"node": "a", "ok": true, "links": [
                    {"neighbor": "b", "type": "RF", "snr": 25.0 - 5.0 * (8.0 - age) / 7.0, "quality": 95.0}
                ]}),
                t,
            );
            let tx = db.unchecked_transaction().unwrap();
            for p in PROJECTIONS {
                p.apply(&tx, &e, t).unwrap();
            }
            tx.commit().unwrap();
            t += 30 * 60_000;
        }
        db.execute(
            "INSERT INTO wx_points (node, lat, lon, station, forecast, ts) VALUES ('a', 35.3, -120.6, 'KSBP', NULL, ?1)",
            [now],
        )
        .unwrap();
    }

    fn drift(app: &App) -> Option<Alert> {
        active_alerts(app, &app.view("a").unwrap())
            .into_iter()
            .find(|a| a.key == "link a|b Drift")
    }

    #[test]
    fn drift_alerts_wait_for_dry_weather() {
        let dir = tempfile::tempdir().unwrap();
        let plain = app(&dir.path().join("plain"), "");
        drifting(&plain);
        assert!(
            drift(&plain).is_some(),
            "no weather configured: it goes out"
        );

        let a = app(
            &dir.path().join("wx"),
            "[weather]
user_agent = \"test (ops@example.org)\"",
        );
        drifting(&a);
        assert!(drift(&a).is_none(), "held until KSBP has been checked");
        let held = |a: &App| keep_open(&a.view("a").unwrap(), &BTreeSet::new(), "link a|b Drift");
        assert!(held(&a), "an open drift alert stays open while held");

        a.wx.observed("KSBP", true, Some(now_ms()), now_ms());
        a.invalidate();
        let out = drift(&a).unwrap();
        assert!(out.text.contains("dry for 24 h at KSBP"), "{}", out.text);
        assert!(!held(&a));

        a.db()
            .execute(
                "INSERT INTO wx_rain (station, ts, mm, text) VALUES ('KSBP', ?1, 1.2, 'Rain')",
                [now_ms() - 2 * HOUR],
            )
            .unwrap();
        a.invalidate();
        assert!(drift(&a).is_none(), "rain at the station holds it again");
        assert!(held(&a));
    }

    #[test]
    fn a_lost_link_stays_open_while_its_only_reporter_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let a = app(dir.path(), "");
        let now = now_ms();
        let mut f = Fake::new(vec![]);
        let links = json!([{"neighbor": "b", "type": "RF", "snr": 22.0, "quality": 95.0}]);
        for (t, l) in [
            (now - 3 * HOUR, links),
            (now - 150 * 60_000, json!([])),
            (now - 140 * 60_000, json!([])),
        ] {
            let e = f.add("node.poll", json!({"node": "a", "ok": true, "links": l}), t);
            let db = a.db();
            let tx = db.unchecked_transaction().unwrap();
            for p in PROJECTIONS {
                p.apply(&tx, &e, t).unwrap();
            }
            tx.commit().unwrap();
        }
        let v = a.view("a").unwrap();
        assert_eq!(v.pairs[0].state, LinkState::Unknown);
        assert!(
            keep_open(&v, &BTreeSet::new(), "link a|b Loss"),
            "no news is not the link coming back"
        );
    }

    fn add(app: &App, kind: &str, payload: serde_json::Value, t: i64) {
        let e = Fake::new(vec![]).add(kind, payload, t);
        let db = app.db();
        let tx = db.unchecked_transaction().unwrap();
        for p in PROJECTIONS {
            p.apply(&tx, &e, t).unwrap();
        }
        tx.commit().unwrap();
    }

    #[test]
    fn below_plan_stays_off_the_alert_channels() {
        let dir = tempfile::tempdir().unwrap();
        let a = app(dir.path(), "");
        drifting(&a);
        let now = now_ms();
        add(
            &a,
            "planner.prediction",
            json!({"a": "a", "b": "b", "snr_db": 40.0}),
            now,
        );
        let v = a.view("a").unwrap();
        assert!(
            v.pairs[0]
                .alerts
                .iter()
                .any(|x| x.kind == AlertKind::BelowPlan)
        );
        assert!(
            active_alerts(&a, &v)
                .iter()
                .all(|x| !x.key.ends_with("BelowPlan"))
        );
    }

    #[test]
    fn an_aim_alert_stays_open_until_the_link_is_back_or_worked_on() {
        let dir = tempfile::tempdir().unwrap();
        let a = app(dir.path(), "");
        drifting(&a);
        let now = now_ms();
        // a step from 30 dB, three weeks ago: too old to see, and the link is near 20 now
        record::note_aims(&a.db(), &[("a".into(), "b".into(), 30.0, now - 21 * DAY)]).unwrap();
        let v = a.view("a").unwrap();
        let off = aims_off(&a, &v).unwrap();
        assert!(keep_open(&v, &off, "link a|b Aim"));
        assert!(!keep_open(&v, &BTreeSet::new(), "link a|b Aim"));
        add(
            &a,
            "link.note",
            json!({"node": "a", "other": "b", "text": "realigned", "change": true}),
            now - HOUR,
        );
        let off = aims_off(&a, &v).unwrap();
        assert!(off.is_empty(), "work on the link ends it");
        assert!(record::aim_steps(&a.db()).unwrap().is_empty());

        record::note_aims(
            &a.db(),
            &[("a".into(), "b".into(), 21.0, now - 30 * 60_000)],
        )
        .unwrap();
        assert!(
            aims_off(&a, &v).unwrap().is_empty(),
            "back within half a step"
        );
    }
}
