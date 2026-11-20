//! The weather cross-check before drift and aim alerts go out. Rain reported by the NWS
//! station nearest either end (api.weather.gov, while the internet is up), rain forecast
//! for the hour, or a wet-antenna, front or ducting candidate from the weather app
//! (spec 09, which needs no internet) holds the alert until it has been dry for `dry_hours`: a wet
//! radome or a rain fade can look like a link getting worse. Rain spells and candidates
//! are also marked on the link charts, and the forecast shows on the workday list.
//!
//! api.weather.gov, as it answered on 2026-11-19: `GET /points/{lat},{lon}` has
//! `properties.observationStations` (a URL listing stations nearest first, each with
//! `properties.stationIdentifier`) and `properties.forecastHourly` (a URL whose
//! `properties.periods` have `startTime`, `endTime` in local time with an offset,
//! `shortForecast` and `probabilityOfPrecipitation.value` in percent).
//! `GET /stations/{id}/observations?start=` has features whose `properties` carry
//! `timestamp`, `textDescription`, `presentWeather[].weather` and, on the hourly reports,
//! `precipitationLastHour` with a `unitCode` of "wmoUnit:mm".
//!
//! The weather app publishes each candidate as a signed `weather.candidate` event in the
//! main log, so every site gets them; its `GET /api/events` (`{"candidates": [...]}`) is
//! the fallback for a site whose core doesn't carry them. Either way a candidate is
//! tagged by `kind`: "wet" with `site`, `start`, `end` (null while it lasts) and `drops`
//! of `{a, b, ...}`; "front" with `drops` the same way; or "duct" with `links` as
//! `[a, b]` pairs. A wet site holds every link there; a front or duct only its links.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::graph::{Edge, edge};
use ridgewarden_client::util::{
    ms_to_rfc3339, norm_node, now_ms, rfc3339_to_ms, short_time, valid_node,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::App;
use crate::config::WeatherCfg;
use crate::history::{DAY, HOUR};
use crate::links::AlertKind;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS wx_points (
    node     TEXT PRIMARY KEY,
    lat      REAL NOT NULL,
    lon      REAL NOT NULL,
    station  TEXT,
    forecast TEXT,
    ts       INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS wx_rain (
    station TEXT NOT NULL,
    ts      INTEGER NOT NULL,
    mm      REAL,
    text    TEXT,
    PRIMARY KEY (station, ts)
);";

const MIN: i64 = 60_000;
const ROUND: Duration = Duration::from_secs(600);
/// a node's nearest station and forecast grid are looked up again after this, or if it moves
const POINT_KEEP: i64 = 30 * DAY;
/// new lookups per round, so a fresh install on a big mesh doesn't fire off dozens at once
const POINTS_PER_ROUND: usize = 10;
const OBS_EVERY: i64 = 30 * MIN;
/// a station is only taken to say "dry" if it reported this recently
const REPORT_FRESH: i64 = 3 * HOUR;
/// a node with no NWS point (outside the US, or the lookup failed) is tried again after this
const POINT_RETRY: i64 = DAY;
const FORECAST_EVERY: i64 = HOUR;
const RAIN_KEEP: i64 = 400 * DAY;
/// rain reports further apart than this are separate spells on a chart
const SPELL_GAP: i64 = 3 * HOUR;
/// a forecast this likely to rain this hour holds alerts like a report of rain would
const RAIN_NOW_POP: f64 = 60.0;
const MAX_CANDIDATES: usize = 500;
/// candidates kept from the log
const MAX_LOGGED: i64 = 10_000;
/// an open candidate that hasn't been ended by now is taken as abandoned
const CAND_STALE: i64 = 3 * DAY;
const WET_WORDS: &[&str] = &[
    "rain",
    "drizzle",
    "shower",
    "thunderstorm",
    "snow",
    "sleet",
    "freezing",
    "ice",
];

pub fn station_ok(s: &str) -> bool {
    (3..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn wet_words(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    WET_WORDS.iter().any(|w| t.contains(w))
}

fn text(v: &Value, k: &str, max: usize) -> Option<String> {
    let t: String = v.get(k)?.as_str()?.chars().take(max).collect();
    (!t.trim().is_empty()).then_some(t)
}

/// A report of rain (or snow) at a station.
#[derive(Clone, Debug, PartialEq)]
pub struct Rain {
    pub ts: i64,
    /// over the hour before, on the hourly reports
    pub mm: Option<f64>,
    pub text: Option<String>,
}

/// The reports in an observations response that say precipitation fell.
pub fn parse_rain(v: &Value) -> Vec<Rain> {
    let features = v.get("features").and_then(Value::as_array);
    features
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let p = f.get("properties")?;
            let ts = rfc3339_to_ms(p.get("timestamp")?.as_str()?)?;
            let mm = p.get("precipitationLastHour").and_then(|q| {
                let v = q.get("value")?.as_f64()?;
                let mm = match q.get("unitCode")?.as_str()? {
                    "wmoUnit:mm" => v,
                    "wmoUnit:m" => v * 1000.0,
                    _ => return None,
                };
                (0.0..1000.0).contains(&mm).then_some(mm)
            });
            let desc = text(p, "textDescription", 80);
            let present = p
                .get("presentWeather")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|w| w.get("weather")?.as_str())
                .any(wet_words);
            let wet =
                mm.is_some_and(|m| m > 0.0) || present || desc.as_deref().is_some_and(wet_words);
            wet.then_some(Rain { ts, mm, text: desc })
        })
        .collect()
}

/// The time of the newest report in an observations response.
pub fn newest(v: &Value) -> Option<i64> {
    v.get("features")?
        .as_array()?
        .iter()
        .filter_map(|f| rfc3339_to_ms(f.pointer("/properties/timestamp")?.as_str()?))
        .max()
}

/// The station list and hourly forecast URLs for a point, if they are on `base`.
pub fn parse_point(v: &Value, base: &str) -> Option<(String, String)> {
    let p = v.get("properties")?;
    let base = base.trim_end_matches('/');
    let url = |k: &str| {
        p.get(k)?
            .as_str()
            .filter(|u| u.strip_prefix(base).is_some_and(|r| r.starts_with('/')) && u.len() < 300)
            .map(str::to_string)
    };
    Some((url("observationStations")?, url("forecastHourly")?))
}

/// The nearest station in a station list.
pub fn parse_station(v: &Value) -> Option<String> {
    v.get("features")?
        .as_array()?
        .iter()
        .filter_map(|f| f.pointer("/properties/stationIdentifier")?.as_str())
        .find(|s| station_ok(s))
        .map(str::to_string)
}

/// One hour of a forecast.
#[derive(Clone, Debug, PartialEq)]
pub struct Period {
    pub start: i64,
    pub end: i64,
    /// the local date the period starts on, "2026-11-19"
    pub day: String,
    pub pop: f64,
    pub text: String,
}

impl Period {
    fn wet(&self) -> bool {
        self.pop >= RAIN_NOW_POP && wet_words(&self.text)
    }
}

pub fn parse_hourly(v: &Value) -> Vec<Period> {
    let periods = v.pointer("/properties/periods").and_then(Value::as_array);
    periods
        .into_iter()
        .flatten()
        .take(200)
        .filter_map(|p| {
            let s = p.get("startTime")?.as_str()?;
            Some(Period {
                start: rfc3339_to_ms(s)?,
                end: rfc3339_to_ms(p.get("endTime")?.as_str()?)?,
                day: s.get(..10)?.to_string(),
                pop: p
                    .pointer("/probabilityOfPrecipitation/value")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .clamp(0.0, 100.0),
                text: text(p, "shortForecast", 60).unwrap_or_default(),
            })
        })
        .collect()
}

/// What the weather app thinks it saw.
#[derive(Clone, Debug, PartialEq)]
pub enum CandKind {
    /// water on the antennas at one site: every link there is affected
    Wet(String),
    /// links dropping across a wide area: only the links that dropped
    Front,
    Duct,
}

/// A wet-antenna, front or ducting candidate from the weather app.
#[derive(Clone, Debug, PartialEq)]
pub struct Cand {
    pub kind: CandKind,
    pub start: i64,
    /// None while it is still going
    pub end: Option<i64>,
    pub links: BTreeSet<Edge>,
}

fn node(v: &Value) -> Option<String> {
    let n = norm_node(v.as_str()?);
    valid_node(&n).then_some(n)
}

fn pairs(list: &Value, pair: fn(&Value) -> Option<(&Value, &Value)>) -> Option<BTreeSet<Edge>> {
    Some(
        list.as_array()?
            .iter()
            .take(256)
            .filter_map(|x| {
                let (a, b) = pair(x)?;
                Some(edge(&node(a)?, &node(b)?))
            })
            .collect(),
    )
}

/// One candidate as the weather app lists it on `/api/events` and in its
/// `weather.candidate` events.
pub fn parse_candidate(c: &Value) -> Option<Cand> {
    let drops = |c: &Value| pairs(c.get("drops")?, |d| Some((d.get("a")?, d.get("b")?)));
    let (kind, links) = match c.get("kind")?.as_str()? {
        "wet" => (CandKind::Wet(node(c.get("site")?)?), drops(c)?),
        "front" => (CandKind::Front, drops(c)?),
        "duct" => (
            CandKind::Duct,
            pairs(c.get("links")?, |l| Some((l.get(0)?, l.get(1)?)))?,
        ),
        _ => return None,
    };
    // a front or duct with no usable link says nothing about any link
    if links.is_empty() && !matches!(kind, CandKind::Wet(_)) {
        return None;
    }
    Some(Cand {
        kind,
        start: c.get("start")?.as_i64()?,
        end: c.get("end").and_then(Value::as_i64),
        links,
    })
}

pub fn parse_candidates(v: &Value) -> Vec<Cand> {
    let list = v.get("candidates").and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .take(MAX_CANDIDATES)
        .filter_map(parse_candidate)
        .collect()
}

impl Cand {
    fn touches(&self, a: &str, b: &str) -> bool {
        self.links.contains(&edge(a, b))
            || matches!(&self.kind, CandKind::Wet(s) if s == a || s == b)
    }

    fn what(&self) -> String {
        match &self.kind {
            CandKind::Wet(s) => format!("wet antennas at {s}"),
            CandKind::Front => "links dropping across a wide area (a front passing)".into(),
            CandKind::Duct => "ducting or fading".into(),
        }
    }

    /// Whether it says the weather was at work on the a-b link between `from` and `now`.
    /// One still open after `CAND_STALE` is taken as abandoned, not as days of rain.
    fn holds(&self, (a, b): (&str, &str), from: i64, now: i64) -> bool {
        let open = match self.end {
            Some(e) => e >= from,
            None => self.start >= now - CAND_STALE,
        };
        self.touches(a, b) && open && self.start <= now
    }
}

const CANDS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS wx_cands (
    author    TEXT NOT NULL,
    id        TEXT NOT NULL,
    ts        INTEGER NOT NULL,
    kind      TEXT NOT NULL,
    site      TEXT,
    start     INTEGER NOT NULL,
    end       INTEGER,
    links     TEXT NOT NULL,
    withdrawn INTEGER NOT NULL,
    PRIMARY KEY (author, id)
);";

/// `weather.candidate` events from the weather app (see its `publish.rs`): the candidate
/// as `/api/events` lists it, plus `v` (1), `id` (16 hex digits) and `withdrawn`. Each
/// is republished under the same id when it ends or is withdrawn; the latest wins.
pub struct Candidates;

impl Projection for Candidates {
    fn schema(&self) -> &'static str {
        CANDS_SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM wx_cands", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "weather.candidate" {
            return Ok(());
        }
        let p = &e.payload;
        let id = p
            .get("id")
            .and_then(Value::as_str)
            .filter(|i| i.len() == 16 && i.bytes().all(|b| b.is_ascii_hexdigit()));
        let (Some(id), Some(1), Some(c)) =
            (id, p.get("v").and_then(Value::as_u64), parse_candidate(p))
        else {
            return Ok(());
        };
        let (kind, site) = match &c.kind {
            CandKind::Wet(s) => ("wet", Some(s.as_str())),
            CandKind::Front => ("front", None),
            CandKind::Duct => ("duct", None),
        };
        tx.execute(
            "INSERT INTO wx_cands (author, id, ts, kind, site, start, end, links, withdrawn)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(author, id) DO UPDATE SET ts = excluded.ts, kind = excluded.kind,
                 site = excluded.site, start = excluded.start, end = excluded.end,
                 links = excluded.links, withdrawn = excluded.withdrawn
             WHERE excluded.ts >= wx_cands.ts",
            params![
                e.author.key,
                id,
                ts,
                kind,
                site,
                c.start,
                c.end,
                serde_json::to_string(&c.links)?,
                p.get("withdrawn").and_then(Value::as_bool).unwrap_or(false)
            ],
        )?;
        tx.execute(
            "DELETE FROM wx_cands WHERE start < ?1 OR rowid IN
               (SELECT rowid FROM wx_cands ORDER BY start DESC LIMIT -1 OFFSET ?2)",
            params![ts - RAIN_KEEP, MAX_LOGGED],
        )?;
        Ok(())
    }
}

/// Whether any `weather.candidate` event has reached this site.
fn feed_seen(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM wx_cands)", [], |r| r.get(0))?)
}

/// Candidates from the log that overlap `from` to `to`, leaving out withdrawn ones.
fn feed_cands(conn: &Connection, from: i64, to: i64) -> Result<Vec<Cand>> {
    let mut st = conn.prepare(
        "SELECT kind, site, start, end, links FROM wx_cands
         WHERE withdrawn = 0 AND start <= ?2 AND (end IS NULL OR end >= ?1)",
    )?;
    let rows = st.query_map(params![from, to], |r| {
        let kind = match (
            r.get::<_, String>(0)?.as_str(),
            r.get::<_, Option<String>>(1)?,
        ) {
            ("wet", Some(s)) => CandKind::Wet(s),
            ("front", _) => CandKind::Front,
            _ => CandKind::Duct,
        };
        let (start, end, links): (i64, Option<i64>, String) = (r.get(2)?, r.get(3)?, r.get(4)?);
        Ok(serde_json::from_str(&links).ok().map(|links| Cand {
            kind,
            start,
            end,
            links,
        }))
    })?;
    let rows: Vec<Option<Cand>> = rows.collect::<Result<_, _>>()?;
    Ok(rows.into_iter().flatten().collect())
}

/// How one source fared at its last attempt.
#[derive(Clone, Debug, Default)]
struct Source {
    tried: bool,
    error: Option<String>,
}

#[derive(Default)]
struct State {
    app: Source,
    cands: Vec<Cand>,
    /// node -> (fetched, hourly forecast)
    forecasts: BTreeMap<String, (i64, Vec<Period>)>,
    stations: BTreeMap<String, Station>,
}

/// How fetching one station's observations has gone.
#[derive(Clone, Debug, Default)]
struct Station {
    tried: Option<i64>,
    /// the last fetch that worked
    fetched: Option<i64>,
    /// the newest report seen, rain or not
    newest: Option<i64>,
    failed: bool,
}

/// What the weather check knows, shared between its loop and the pages.
#[derive(Default)]
pub struct Wx(Mutex<State>);

/// The verdict on one drift or aim alert.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    /// why the alert is held back, if it is
    pub held: Option<String>,
    /// what was checked, for the alert text and the link page
    pub note: String,
}

/// Which alert kinds the check applies to: the slow ones that weather can imitate.
pub fn applies(kind: AlertKind) -> bool {
    matches!(kind, AlertKind::Drift | AlertKind::Aim)
}

fn station_for(conn: &Connection, cfg: &WeatherCfg, node: &str) -> Result<Option<String>> {
    if let Some(s) = cfg.station(node) {
        return Ok(Some(s.to_string()));
    }
    Ok(conn
        .query_row(
            "SELECT station FROM wx_points WHERE node = ?1",
            [node],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

fn rain_words(r: &Rain) -> String {
    let mut parts = vec![];
    if let Some(t) = &r.text {
        parts.push(t.clone());
    }
    if let Some(mm) = r.mm.filter(|m| *m > 0.0) {
        parts.push(format!("{mm:.1} mm in the hour"));
    }
    parts.join(", ")
}

impl Wx {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the weather allows a drift or aim alert on the a-b link to go out now.
    /// None when no weather source is configured.
    pub fn check(
        &self,
        conn: &Connection,
        cfg: &WeatherCfg,
        (a, b): (&str, &str),
        now: i64,
    ) -> Result<Option<Check>> {
        let feed = cfg.feed && feed_seen(conn)?;
        if !cfg.on() && !feed {
            return Ok(None);
        }
        let dry = cfg.dry_hours as i64 * HOUR;
        let st = self.state();
        let mut held = vec![];
        let mut checked = vec![];
        let mut notes = vec![];
        if cfg.user_agent.is_some() {
            let mut stations = BTreeSet::new();
            for n in [a, b] {
                match station_for(conn, cfg, n)? {
                    Some(s) => {
                        stations.insert(s);
                    }
                    None => notes.push(format!(
                        "No NWS station is known yet for {n} (it needs a position in its polls, or a station under [weather.stations])."
                    )),
                }
            }
            for s in &stations {
                let Some(info) = st.stations.get(s).filter(|i| i.tried.is_some()) else {
                    held.push(format!("waiting for the first NWS check at {s}"));
                    continue;
                };
                let last: Option<Rain> = conn
                    .query_row(
                        "SELECT ts, mm, text FROM wx_rain WHERE station = ?1 AND ts >= ?2 AND ts <= ?3
                         ORDER BY ts DESC LIMIT 1",
                        params![s, now - dry, now],
                        |r| Ok(Rain { ts: r.get(0)?, mm: r.get(1)?, text: r.get(2)? }),
                    )
                    .optional()?;
                match last {
                    Some(r) => held.push(format!(
                        "rain at {s} at {} ({})",
                        short_time(r.ts),
                        rain_words(&r)
                    )),
                    // dry only on the strength of a recent report, not a silent station
                    None if info.newest.is_some_and(|t| now - t < REPORT_FRESH) => {
                        checked.push(s.clone())
                    }
                    None if info.failed => notes.push(format!(
                        "The last NWS check at {s} failed, so rain there wasn't checked."
                    )),
                    None => notes.push(format!(
                        "No recent report from {s}, so rain there wasn't checked."
                    )),
                }
            }
            for n in [a, b] {
                if let Some(p) = st
                    .forecasts
                    .get(n)
                    .and_then(|(_, ps)| ps.iter().find(|p| p.start <= now && now < p.end))
                    .filter(|p| p.wet())
                {
                    held.push(format!(
                        "{} forecast near {n} this hour ({:.0}%)",
                        p.text, p.pop
                    ));
                }
            }
        }
        if feed || cfg.app_url.is_some() {
            // the log carries the candidates when it can; the app's own page is the fallback
            if !feed && !st.app.tried {
                held.push("waiting for the first check with the weather app".to_string());
            }
            let mut cands = if feed {
                feed_cands(conn, now - dry, now)?
            } else {
                vec![]
            };
            cands.extend(st.cands.iter().cloned());
            let mut seen = BTreeSet::new();
            for c in cands.iter().filter(|c| c.holds((a, b), now - dry, now)) {
                let what = format!(
                    "the weather app saw {} from {}",
                    c.what(),
                    short_time(c.start)
                );
                if seen.insert(what.clone()) {
                    held.push(what);
                }
            }
            if feed || (st.app.tried && st.app.error.is_none()) {
                checked.push("the weather app".into());
            } else if st.app.error.is_some() {
                notes.push("The weather app could not be reached at its last check.".into());
            }
        }
        let hours = cfg.dry_hours;
        let held = (!held.is_empty()).then(|| {
            format!(
                "Held back: {}. Water on an antenna or a rain fade can look like this, so it goes out once it has been dry for {hours} h.",
                held.join("; ")
            )
        });
        if !checked.is_empty() && held.is_none() {
            notes.insert(
                0,
                format!(
                    "Checked against the weather: dry for {hours} h at {}.",
                    checked.join(" and ")
                ),
            );
        }
        Ok(Some(Check {
            held,
            note: notes.join(" "),
        }))
    }

    /// Chart markers for the a-b link between `from` and `to`: rain spells at the
    /// stations for either end, and weather app candidates touching the link.
    pub fn marks(
        &self,
        conn: &Connection,
        cfg: &WeatherCfg,
        (a, b): (&str, &str),
        from: i64,
        to: i64,
    ) -> Result<Vec<(i64, String)>> {
        let mut out = vec![];
        let mut stations = BTreeSet::new();
        for n in [a, b] {
            stations.extend(station_for(conn, cfg, n)?);
        }
        for s in &stations {
            let mut st = conn.prepare(
                "SELECT ts, mm FROM wx_rain WHERE station = ?1 AND ts >= ?2 AND ts <= ?3 ORDER BY ts",
            )?;
            let rows: Vec<(i64, Option<f64>)> = st
                .query_map(params![s, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (start, end, mm) in spells(&rows) {
                let mut t = format!("rain at {s}");
                if end - start >= HOUR {
                    t += &format!(" for {} h", (end - start + HOUR / 2) / HOUR);
                }
                if mm > 0.0 {
                    t += &format!(", {mm:.1} mm");
                }
                out.push((start, t));
            }
        }
        let mut cands = if cfg.feed {
            feed_cands(conn, from, to)?
        } else {
            vec![]
        };
        cands.extend(self.state().cands.iter().cloned());
        let mut seen = BTreeSet::new();
        for c in cands.iter().filter(|c| c.touches(a, b)) {
            let m = (c.start, format!("weather app: {}", c.what()));
            if c.start >= from && c.start <= to && seen.insert(m.clone()) {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// The next three days' forecast near `node`, if it has been fetched.
    pub fn outlook(&self, node: &str, now: i64) -> Option<String> {
        let st = self.state();
        let (_, periods) = st.forecasts.get(node)?;
        let mut days: Vec<(&str, &Period)> = vec![];
        for p in periods.iter().filter(|p| p.end > now) {
            if let Some((d, best)) = days.last_mut()
                && *d == p.day
            {
                if p.pop > best.pop {
                    *best = p;
                }
            } else if days.len() == 3 {
                break;
            } else {
                days.push((&p.day, p));
            }
        }
        if days.is_empty() {
            return None;
        }
        let words: Vec<String> = days
            .iter()
            .map(|(d, p)| {
                format!(
                    "{} {} ({:.0}% rain)",
                    weekday(d).unwrap_or(d),
                    p.text,
                    p.pop
                )
            })
            .collect();
        Some(words.join(", "))
    }

    /// Record a try at a station's observations: whether it worked, and the newest
    /// report it returned.
    pub(crate) fn observed(&self, station: &str, ok: bool, newest: Option<i64>, now: i64) {
        let mut st = self.state();
        let e = st.stations.entry(station.to_string()).or_default();
        e.tried = Some(now);
        e.failed = !ok;
        if ok {
            e.fetched = Some(now);
            e.newest = e.newest.max(newest.filter(|t| *t <= now));
        }
    }

    fn set_app(&self, r: Result<Vec<Cand>>) {
        let mut st = self.state();
        st.app.tried = true;
        match r {
            Ok(c) => {
                st.cands = c;
                st.app.error = None;
            }
            Err(e) => {
                st.cands.clear();
                st.app.error = Some(format!("{e:#}"));
            }
        }
    }
}

/// Rain reports grouped into spells: (start, last report, mm). Special reports between
/// the hourly ones carry the total since the last hourly report, so each clock hour adds
/// only its largest figure.
fn spells(rows: &[(i64, Option<f64>)]) -> Vec<(i64, i64, f64)> {
    let mut out: Vec<(i64, i64, BTreeMap<i64, f64>)> = vec![];
    for &(ts, mm) in rows {
        let hour = ts - ts.rem_euclid(HOUR);
        let add = |m: &mut BTreeMap<i64, f64>| {
            let e = m.entry(hour).or_default();
            *e = e.max(mm.unwrap_or(0.0));
        };
        match out.last_mut() {
            Some(s) if ts - s.1 <= SPELL_GAP => {
                s.1 = ts;
                add(&mut s.2);
            }
            _ => {
                let mut m = BTreeMap::new();
                add(&mut m);
                out.push((ts, ts, m));
            }
        }
    }
    out.into_iter()
        .map(|(a, b, m)| (a, b, m.values().sum()))
        .collect()
}

/// "Thu" for "2026-11-19".
fn weekday(date: &str) -> Option<&'static str> {
    let mut it = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (it.next()??, it.next()??, it.next()??);
    // days since 1970-01-01 by Howard Hinnant's days_from_civil; that day was a Thursday
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize])
}

async fn get_json(http: &reqwest::Client, url: &str, ua: Option<&str>) -> Result<Value> {
    let mut req = http.get(url);
    if let Some(ua) = ua {
        req = req
            .header(reqwest::header::USER_AGENT, ua)
            .header(reqwest::header::ACCEPT, "application/geo+json");
    }
    let r = req.send().await?;
    if !r.status().is_success() {
        bail!("{url} answered {}", r.status());
    }
    Ok(r.json().await?)
}

/// Where each node polled in the last week says it is.
fn positions(conn: &Connection, now: i64) -> Result<Vec<(String, f64, f64)>> {
    let mut st = conn.prepare(
        "SELECT node, lat, lon, max(ts) FROM polls WHERE ok = 1 AND ts > ?1
         AND lat BETWEEN -90 AND 90 AND lon BETWEEN -180 AND 180 GROUP BY node",
    )?;
    let rows = st.query_map([now - 7 * DAY], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Nodes whose nearest station and forecast grid need looking up.
fn stale_points(conn: &Connection, now: i64) -> Result<Vec<(String, f64, f64)>> {
    let mut out = vec![];
    for (node, lat, lon) in positions(conn, now)? {
        let known: Option<(f64, f64, i64, bool)> = conn
            .query_row(
                "SELECT lat, lon, ts, forecast IS NOT NULL FROM wx_points WHERE node = ?1",
                [&node],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let fresh = known.is_some_and(|(la, lo, ts, found)| {
            let keep = if found { POINT_KEEP } else { POINT_RETRY };
            now - ts < keep && (la - lat).abs() < 0.01 && (lo - lon).abs() < 0.01
        });
        if !fresh {
            out.push((node, lat, lon));
        }
    }
    Ok(out)
}

/// Look up, fetch and store what the NWS has for the mesh: the nearest stations and
/// forecast grids for nodes that report a position, rain at those stations, and the
/// hourly forecast for `wanted` nodes. Returns the first failure, after trying the rest.
async fn nws_round(app: &App, ua: &str, wanted: &BTreeSet<String>) -> Result<()> {
    let cfg = &app.cfg.weather;
    let base = cfg.nws_url.trim_end_matches('/');
    let now = now_ms();
    let mut first_err: Option<anyhow::Error> = None;
    let mut fail = |e: anyhow::Error| {
        tracing::debug!("weather: {e:#}");
        first_err.get_or_insert(e);
    };
    let mut todo = stale_points(&app.db(), now)?;
    todo.sort_by_key(|(n, _, _)| !wanted.contains(n));
    for (node, lat, lon) in todo.into_iter().take(POINTS_PER_ROUND) {
        let found = async {
            // two decimals (about a kilometre) is plenty to find a station and grid
            let p = get_json(
                &app.http,
                &format!("{base}/points/{lat:.2},{lon:.2}"),
                Some(ua),
            )
            .await?;
            let Some((stations, forecast)) = parse_point(&p, base) else {
                bail!("api.weather.gov has no forecast point for {node}");
            };
            let station = parse_station(&get_json(&app.http, &stations, Some(ua)).await?);
            anyhow::Ok((station, forecast))
        }
        .await;
        match found {
            Ok((station, forecast)) => {
                app.db().execute(
                    "INSERT OR REPLACE INTO wx_points (node, lat, lon, station, forecast, ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![node, lat, lon, station, forecast, now],
                )?;
            }
            Err(e) => {
                // remembered, so a node outside the US isn't asked about every round; a
                // station found earlier is kept
                app.db().execute(
                    "INSERT INTO wx_points (node, lat, lon, station, forecast, ts)
                     VALUES (?1, ?2, ?3, NULL, NULL, ?4)
                     ON CONFLICT(node) DO UPDATE SET lat = excluded.lat, lon = excluded.lon, ts = excluded.ts",
                    params![node, lat, lon, now],
                )?;
                fail(e.context(format!("looking up {node}")));
            }
        }
    }

    let stations: BTreeSet<String> = {
        let db = app.db();
        let mut st = db.prepare("SELECT station FROM wx_points WHERE station IS NOT NULL")?;
        let found: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        found
            .into_iter()
            .chain(cfg.stations.values().cloned())
            .collect()
    };
    let back = cfg.dry_hours as i64 * HOUR;
    for s in stations {
        let info = app.wx.state().stations.get(&s).cloned().unwrap_or_default();
        if info.tried.is_some_and(|t| now - t < OBS_EVERY) {
            continue;
        }
        let start = ms_to_rfc3339(info.fetched.map_or(now - back, |t| t - 3 * HOUR));
        let url = format!("{base}/stations/{s}/observations?start={start}");
        match get_json(&app.http, &url, Some(ua)).await {
            Ok(v) => {
                let db = app.db();
                for r in parse_rain(&v).into_iter().filter(|r| r.ts <= now) {
                    db.execute(
                        "INSERT OR REPLACE INTO wx_rain (station, ts, mm, text) VALUES (?1, ?2, ?3, ?4)",
                        params![s, r.ts, r.mm, r.text],
                    )?;
                }
                drop(db);
                app.wx.observed(&s, true, newest(&v), now);
            }
            Err(e) => {
                app.wx.observed(&s, false, None, now);
                fail(e.context(format!("observations at {s}")));
            }
        }
    }

    // one request per forecast grid, however many wanted nodes share it
    let mut grids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in wanted {
        let fetched = app.wx.state().forecasts.get(node).map(|f| f.0);
        if fetched.is_some_and(|t| now - t < FORECAST_EVERY) {
            continue;
        }
        let url: Option<String> = app
            .db()
            .query_row(
                "SELECT forecast FROM wx_points WHERE node = ?1",
                [node],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(url) = url {
            grids.entry(url).or_default().push(node.clone());
        }
    }
    for (url, nodes) in grids {
        match get_json(&app.http, &url, Some(ua)).await {
            Ok(v) => {
                let periods = parse_hourly(&v);
                let mut st = app.wx.state();
                for n in nodes {
                    st.forecasts.insert(n, (now, periods.clone()));
                }
            }
            Err(e) => fail(e.context(format!("forecast for {}", nodes.join(", ")))),
        }
    }
    app.db()
        .execute("DELETE FROM wx_rain WHERE ts < ?1", [now - RAIN_KEEP])?;
    first_err.map_or(Ok(()), Err)
}

/// Sites worth a forecast: the ends of drift and aim alerts, and the workday list's sites.
async fn wanted(app: &App) -> Result<BTreeSet<String>> {
    let here = app.here().await;
    let v = app.view(&here)?;
    let mut out: BTreeSet<String> = v
        .pairs
        .iter()
        .filter(|p| p.alerts.iter().any(|a| applies(a.kind)))
        .flat_map(|p| [p.a.clone(), p.b.clone()])
        .collect();
    out.extend(v.workday.iter().map(|w| w.site.clone()));
    Ok(out)
}

async fn round(app: &App) {
    let cfg = &app.cfg.weather;
    if let Some(url) = &cfg.app_url {
        let r = get_json(
            &app.http,
            &format!("{}/api/events", url.trim_end_matches('/')),
            None,
        )
        .await
        .map(|v| parse_candidates(&v));
        if let Err(e) = &r {
            tracing::warn!("weather app at {url}: {e:#}");
        }
        app.wx.set_app(r);
    }
    if let Some(ua) = &cfg.user_agent {
        let r = match wanted(app).await {
            Ok(w) => nws_round(app, ua, &w).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &r {
            tracing::warn!("NWS weather check: {e:#}");
        }
    }
    app.invalidate();
}

/// Keep the weather up to date, forever, if a source is configured.
pub async fn run(app: Arc<App>) {
    if !app.cfg.weather.on() {
        return;
    }
    // let the feed catch up first, so the nodes' positions are known
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        round(&app).await;
        tokio::time::sleep(ROUND).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn reads_rain_from_nws_observations() {
        let v = fixture("nws-observations.json");
        assert_eq!(newest(&v), rfc3339_to_ms("2026-11-20T01:00:00+00:00"));
        let r = parse_rain(&v);
        let at = |s: &str| r.iter().find(|x| x.ts == rfc3339_to_ms(s).unwrap());
        let hourly = at("2026-11-20T00:56:00+00:00").unwrap();
        assert_eq!(hourly.mm, Some(0.5));
        assert_eq!(hourly.text.as_deref(), Some("Light Rain"));
        assert!(
            at("2026-11-20T00:20:00+00:00").is_some(),
            "rain in the text alone"
        );
        assert!(
            at("2026-11-19T23:56:00+00:00").is_none(),
            "a measured zero is dry"
        );
        assert!(at("2026-11-20T00:50:00+00:00").is_none(), "haze is dry");
        assert!(parse_rain(&json!({"title": "Not Found"})).is_empty());
    }

    #[test]
    fn reads_points_stations_and_the_hourly_forecast() {
        let base = "https://api.weather.gov";
        let (stations, hourly) = parse_point(&fixture("nws-point.json"), base).unwrap();
        assert_eq!(
            stations,
            "https://api.weather.gov/gridpoints/LOX/78,115/stations"
        );
        assert!(hourly.ends_with("/forecast/hourly"));
        assert!(parse_point(&fixture("nws-point.json"), "https://elsewhere.example").is_none());
        assert_eq!(
            parse_station(&fixture("nws-stations.json")).as_deref(),
            Some("KSBP")
        );
        let p = parse_hourly(&fixture("nws-hourly.json"));
        assert_eq!(p[0].day, "2026-11-19");
        assert_eq!(p[0].pop, 22.0);
        assert_eq!(p[0].start, rfc3339_to_ms("2026-11-20T00:00:00Z").unwrap());
        assert!(!p[0].wet(), "a slight chance doesn't hold anything");
    }

    #[test]
    fn reads_weather_app_candidates_and_drops_bad_names() {
        let v = json!({"generated": 1, "candidates": [
            {"kind": "wet", "site": "TST-Hub", "start": 100, "end": null, "site_links": 3,
             "others": 2, "others_dropping": 0,
             "drops": [{"a": "tst-hub", "b": "tst-n1", "start": 100, "end": null, "depth": 3.0},
                       {"a": "tst-hub", "b": "../etc", "start": 100, "end": null, "depth": 3.0}]},
            {"kind": "duct", "start": 50, "end": 90, "links": [["tst-n1", "tst-n2"]], "peak_at": 60,
             "peak_departing": 3, "peak_with_data": 5, "peak_up": 3, "peak_down": 0, "max_abs": 6.0},
            {"kind": "hail", "start": 1}
        ]});
        let c = parse_candidates(&v);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].kind, CandKind::Wet("tst-hub".into()));
        assert_eq!(c[0].links.len(), 1);
        assert!(
            c[0].touches("tst-hub", "tst-far"),
            "every link at a wet site"
        );
        assert!(c[1].touches("tst-n2", "tst-n1"));
        assert!(!c[1].touches("tst-n1", "tst-far"));
    }

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(CANDS_SCHEMA).unwrap();
        c.execute(
            "INSERT INTO wx_points VALUES ('tst-a', 35.3, -120.6, 'KSBP', NULL, 0)",
            [],
        )
        .unwrap();
        c
    }

    fn nws() -> WeatherCfg {
        WeatherCfg {
            user_agent: Some("test (ops@example.org)".into()),
            ..Default::default()
        }
    }

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn rain_at_either_end_holds_until_dry() {
        let c = db();
        let wx = Wx::default();
        let cfg = nws();
        let check = |now| {
            wx.check(&c, &cfg, ("tst-a", "tst-b"), now)
                .unwrap()
                .unwrap()
        };
        assert!(
            check(NOW)
                .held
                .unwrap()
                .contains("waiting for the first NWS check at KSBP")
        );
        wx.observed("KSBP", false, None, NOW);
        assert!(check(NOW).note.contains("check at KSBP failed"));
        wx.observed("KSBP", true, Some(NOW - 5 * HOUR), NOW);
        let quiet = check(NOW);
        assert_eq!(quiet.held, None);
        assert!(
            quiet.note.contains("No recent report from KSBP"),
            "a silent station is not a dry one: {}",
            quiet.note
        );
        wx.observed("KSBP", true, Some(NOW - 20 * MIN), NOW);
        let dry = check(NOW);
        assert_eq!(dry.held, None);
        assert!(dry.note.contains("dry for 24 h at KSBP"), "{}", dry.note);
        assert!(
            dry.note.contains("No NWS station is known yet for tst-b"),
            "{}",
            dry.note
        );
        c.execute(
            "INSERT INTO wx_rain VALUES ('KSBP', ?1, 0.5, 'Light Rain')",
            [NOW - 3 * HOUR],
        )
        .unwrap();
        let wet = check(NOW).held.unwrap();
        assert!(
            wet.contains("rain at KSBP") && wet.contains("0.5 mm"),
            "{wet}"
        );
        wx.observed("KSBP", true, Some(NOW + 21 * HOUR), NOW + 22 * HOUR);
        assert_eq!(check(NOW + 22 * HOUR).held, None, "a day after the rain");
        assert!(!WeatherCfg::default().on());
    }

    #[test]
    fn a_wet_forecast_or_candidate_holds_too() {
        let c = db();
        let wx = Wx::default();
        let cfg = WeatherCfg {
            app_url: Some("http://127.0.0.1:8801".into()),
            ..nws()
        };
        wx.observed("KSBP", true, Some(NOW), NOW);
        wx.set_app(Ok(vec![]));
        assert_eq!(
            wx.check(&c, &cfg, ("tst-a", "tst-b"), NOW)
                .unwrap()
                .unwrap()
                .held,
            None
        );
        wx.state().forecasts.insert(
            "tst-b".into(),
            (
                NOW,
                vec![Period {
                    start: NOW - HOUR / 2,
                    end: NOW + HOUR / 2,
                    day: "2027-01-15".into(),
                    pop: 80.0,
                    text: "Rain Likely".into(),
                }],
            ),
        );
        let held = wx
            .check(&c, &cfg, ("tst-a", "tst-b"), NOW)
            .unwrap()
            .unwrap()
            .held
            .unwrap();
        assert!(held.contains("Rain Likely forecast near tst-b"), "{held}");
        wx.state().forecasts.clear();
        wx.set_app(Ok(vec![Cand {
            kind: CandKind::Wet("tst-b".into()),
            start: NOW - 5 * HOUR,
            end: Some(NOW - 2 * HOUR),
            links: BTreeSet::new(),
        }]));
        let held = wx
            .check(&c, &cfg, ("tst-a", "tst-b"), NOW)
            .unwrap()
            .unwrap()
            .held
            .unwrap();
        assert!(held.contains("wet antennas at tst-b"), "{held}");
        wx.set_app(Err(anyhow::anyhow!("connection refused")));
        let after = wx
            .check(&c, &cfg, ("tst-a", "tst-b"), NOW + 2 * DAY)
            .unwrap()
            .unwrap();
        assert_eq!(after.held, None, "an unreachable app holds nothing");
        assert!(after.note.contains("could not be reached"));
    }

    #[test]
    fn rain_spells_and_outlook() {
        // a special report at 0:20 with 0.3 mm so far, then the hourly one at 0:56 with 0.5
        let rows = [
            (20 * MIN, Some(0.3)),
            (56 * MIN, Some(0.5)),
            (HOUR + 56 * MIN, Some(1.0)),
            (2 * HOUR, None),
            (10 * HOUR, None),
        ];
        assert_eq!(
            spells(&rows),
            vec![(20 * MIN, 2 * HOUR, 1.5), (10 * HOUR, 10 * HOUR, 0.0)]
        );
        assert_eq!(weekday("2026-11-19"), Some("Thu"));
        assert_eq!(weekday("2000-02-29"), Some("Tue"));
        let wx = Wx::default();
        let p = |h: i64, day: &str, pop: f64, text: &str| Period {
            start: NOW + h * HOUR,
            end: NOW + (h + 1) * HOUR,
            day: day.into(),
            pop,
            text: text.into(),
        };
        wx.state().forecasts.insert(
            "tst-a".into(),
            (
                NOW,
                vec![
                    p(0, "2026-11-19", 20.0, "Cloudy"),
                    p(1, "2026-11-19", 50.0, "Chance Rain"),
                    p(2, "2026-11-20", 5.0, "Sunny"),
                    p(3, "2026-11-21", 70.0, "Rain Likely"),
                    p(4, "2026-11-22", 0.0, "Sunny"),
                ],
            ),
        );
        assert_eq!(
            wx.outlook("tst-a", NOW).unwrap(),
            "Thu Chance Rain (50% rain), Fri Sunny (5% rain), Sat Rain Likely (70% rain)"
        );
        assert_eq!(wx.outlook("tst-b", NOW), None);
    }

    #[test]
    fn urls_from_a_point_must_stay_on_the_api_host() {
        let base = "https://api.weather.gov";
        for bad in [
            "https://api.weather.gov.evil.example/stations",
            "https://api.weather.gov@evil.example/stations",
            "https://api.weather.govx/stations",
        ] {
            let v = json!({"properties": {"observationStations": bad, "forecastHourly": "https://api.weather.gov/f"}});
            assert!(parse_point(&v, base).is_none(), "{bad}");
        }
        let ok = json!({"properties": {"observationStations": "https://api.weather.gov/s", "forecastHourly": "https://api.weather.gov/f"}});
        assert!(parse_point(&ok, base).is_some());
        assert!(parse_point(&ok, "https://api.weather.gov/").is_some());
    }

    #[test]
    fn rain_dated_in_the_future_holds_nothing() {
        let c = db();
        let wx = Wx::default();
        wx.observed("KSBP", true, Some(NOW), NOW);
        c.execute(
            "INSERT INTO wx_rain VALUES ('KSBP', ?1, 0.5, 'Light Rain')",
            [NOW + 30 * DAY],
        )
        .unwrap();
        let check = wx
            .check(&c, &nws(), ("tst-a", "tst-b"), NOW)
            .unwrap()
            .unwrap();
        assert_eq!(check.held, None);
    }

    #[test]
    fn an_open_candidate_does_not_outlive_an_unreachable_app() {
        let c = db();
        let wx = Wx::default();
        let cfg = WeatherCfg {
            app_url: Some("http://127.0.0.1:8801".into()),
            ..Default::default()
        };
        wx.set_app(Ok(vec![Cand {
            kind: CandKind::Wet("tst-b".into()),
            start: NOW - 5 * HOUR,
            end: None,
            links: BTreeSet::new(),
        }]));
        let held = |now| {
            wx.check(&c, &cfg, ("tst-a", "tst-b"), now)
                .unwrap()
                .unwrap()
                .held
        };
        assert!(held(NOW).is_some());
        wx.set_app(Err(anyhow::anyhow!("connection refused")));
        assert_eq!(held(NOW + HOUR), None, "an unreachable app holds nothing");
    }

    /// The weather app's events, as its publish.rs writes them.
    fn logged(f: &mut ridgewarden_client::mesh::testing::Fake, id: &str, c: Value, ts: i64) {
        let mut p = c;
        p["v"] = json!(1);
        p["id"] = json!(id);
        p["summary"] = json!("a sentence");
        p["evidence"] =
            json!({"rain_stations": [], "rain": null, "strikes": null, "soundings": []});
        if p.get("withdrawn").is_none() {
            p["withdrawn"] = json!(false);
        }
        f.add("weather.candidate", p, ts);
    }

    fn feed_db() -> ridgewarden_client::mesh::testing::Fake {
        let f = ridgewarden_client::mesh::testing::Fake::new(vec![Box::new(Candidates)]);
        f.conn.execute_batch(SCHEMA).unwrap();
        f
    }

    fn held_by_feed(c: &Connection, link: (&str, &str), now: i64) -> Option<String> {
        Wx::default()
            .check(c, &WeatherCfg::default(), link, now)
            .unwrap()
            .and_then(|c| c.held)
    }

    #[test]
    fn candidates_from_the_log_hold_until_withdrawn_or_dry() {
        let mut f = feed_db();
        assert!(
            Wx::default()
                .check(&f.conn, &WeatherCfg::default(), ("tst-a", "tst-b"), NOW)
                .unwrap()
                .is_none(),
            "no weather source at all: nothing to say"
        );
        let wet = json!({"kind": "wet", "site": "tst-b", "start": NOW - 2 * HOUR, "end": null,
            "drops": [{"a": "tst-b", "b": "tst-c", "start": NOW - 2 * HOUR, "end": null,
                       "last_low": NOW, "depth": 3.0}],
            "site_links": 2, "others": 4, "others_dropping": 0});
        logged(&mut f, "00aa00aa00aa00aa", wet.clone(), NOW - HOUR);
        let held = held_by_feed(&f.conn, ("tst-a", "tst-b"), NOW).unwrap();
        assert!(held.contains("wet antennas at tst-b"), "{held}");
        // a later scan withdraws it
        let mut gone = wet.clone();
        gone["withdrawn"] = json!(true);
        gone["end"] = json!(NOW - HOUR / 2);
        logged(&mut f, "00aa00aa00aa00aa", gone, NOW - HOUR / 2);
        // and an old copy arriving late changes nothing
        logged(&mut f, "00aa00aa00aa00aa", wet.clone(), NOW - 3 * HOUR);
        assert_eq!(held_by_feed(&f.conn, ("tst-a", "tst-b"), NOW), None);
        let checked = Wx::default()
            .check(&f.conn, &WeatherCfg::default(), ("tst-a", "tst-b"), NOW)
            .unwrap()
            .unwrap();
        assert!(
            checked.note.contains("dry for 24 h at the weather app"),
            "{}",
            checked.note
        );

        // one that ended keeps holding for dry_hours after
        let mut ended = wet;
        ended["end"] = json!(NOW - 20 * HOUR);
        logged(&mut f, "11bb11bb11bb11bb", ended, NOW - 20 * HOUR);
        assert!(held_by_feed(&f.conn, ("tst-a", "tst-b"), NOW).is_some());
        assert_eq!(
            held_by_feed(&f.conn, ("tst-a", "tst-b"), NOW + 5 * HOUR),
            None
        );
        // with the log turned off it isn't consulted
        let off = WeatherCfg {
            feed: false,
            ..Default::default()
        };
        assert!(
            Wx::default()
                .check(&f.conn, &off, ("tst-a", "tst-b"), NOW)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn fronts_hold_their_drop_links_and_ducts_may_be_ongoing() {
        let mut f = feed_db();
        logged(
            &mut f,
            "22cc22cc22cc22cc",
            json!({"kind": "front", "start": NOW - HOUR, "end": NOW - HOUR / 2,
                "drops": [{"a": "tst-a", "b": "tst-b", "start": NOW - HOUR, "end": NOW - HOUR / 2,
                           "last_low": NOW - HOUR, "depth": 4.0}],
                "sites": ["tst-a", "tst-b"], "with_data": 9}),
            NOW - HOUR / 2,
        );
        assert!(
            held_by_feed(&f.conn, ("tst-b", "tst-a"), NOW)
                .unwrap()
                .contains("front")
        );
        assert_eq!(
            held_by_feed(&f.conn, ("tst-a", "tst-c"), NOW),
            None,
            "only the links that dropped"
        );
        logged(
            &mut f,
            "33dd33dd33dd33dd",
            json!({"kind": "duct", "start": NOW - 2 * HOUR, "end": null, "last": NOW,
                "links": [["tst-x", "tst-y"]], "peak_at": NOW - HOUR, "peak_departing": 3,
                "peak_with_data": 6, "peak_up": 3, "peak_down": 0, "max_abs": 6.0}),
            NOW,
        );
        assert!(
            held_by_feed(&f.conn, ("tst-y", "tst-x"), NOW)
                .unwrap()
                .contains("ducting")
        );
        assert_eq!(
            held_by_feed(&f.conn, ("tst-y", "tst-x"), NOW + 4 * DAY),
            None,
            "never ended: abandoned after a few days"
        );
        // bad ids, versions and names are ignored
        let duct =
            json!({"kind": "duct", "start": NOW, "end": null, "links": [["tst-p", "tst-q"]]});
        logged(&mut f, "not-hex-at-all!!", duct.clone(), NOW);
        logged(
            &mut f,
            "55ff55ff55ff55ff",
            json!({"kind": "duct", "start": NOW, "end": null, "links": [["../x", "tst-q"]]}),
            NOW,
        );
        let mut v2 = duct;
        v2["v"] = json!(2);
        v2["id"] = json!("44ee44ee44ee44ee");
        f.add("weather.candidate", v2, NOW);
        let n: i64 = f
            .conn
            .query_row("SELECT count(*) FROM wx_cands", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let marks = Wx::default()
            .marks(
                &f.conn,
                &WeatherCfg::default(),
                ("tst-a", "tst-b"),
                NOW - DAY,
                NOW,
            )
            .unwrap();
        assert_eq!(marks.len(), 1, "{marks:?}");
        assert!(marks[0].1.contains("front"));
    }
}
