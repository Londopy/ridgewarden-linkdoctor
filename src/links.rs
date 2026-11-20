//! Link Doctor (spec 07): per-link history, slow-drift / flapping / loss / one-way /
//! noise alerts, single points of failure, and a scored "next workday" list grouped by
//! site, all in plain words.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, params};

use ridgewarden_client::graph::{Edge, Graph, edge};
use ridgewarden_client::util::{human_duration, median, short_time};

use crate::config::LinksCfg;
use crate::history::{self, DAY, HOUR, Hour};

/// raw samples loaded for state, flaps and one-way checks
const HISTORY_MS: i64 = 8 * DAY;
/// drift is measured over at most this long, and from the last radio change if later
const DRIFT_WINDOW: i64 = 30 * DAY;
/// a drift fit needs this much history and this many hourly points
const MIN_DRIFT_SPAN: i64 = 3 * DAY;
const MIN_DRIFT_HOURS: usize = 48;
/// share of point pairs that must agree with the fitted direction
const MIN_DRIFT_AGREEMENT: f64 = 0.65;
/// the fit uses at most this many points, so pair counts stay small on long histories
const MAX_FIT_POINTS: usize = 400;
/// one-way and noise checks look at this much recent data
const RECENT: i64 = 6 * HOUR;
/// a step in SNR is looked for in this much history (since the last radio change)
const STEP_WINDOW: i64 = 14 * DAY;
/// hourly points needed on each side of a step
const STEP_HOURS: usize = 24;
/// a noise floor that moved less than this across a step counts as steady
const STEADY_NOISE_DB: f64 = 2.0;
/// how long an AREDN 4.x LQM tracker still lists a neighbor it no longer hears
const TRACKER_LAG: i64 = 120_000;
/// drops this soon after a firmware change or radio work are the work, not flapping
const WORK_MS: i64 = 30 * 60_000;
/// below this SNR (dB) or quality (%) a link counts as weak
pub const WEAK_SNR: f64 = 15.0;
pub const WEAK_QUALITY: f64 = 80.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub ts: i64,
    pub event_id: String,
    pub link_type: Option<String>,
    pub snr: Option<f64>,
    pub signal: Option<f64>,
    pub noise: Option<f64>,
    pub quality: Option<f64>,
    pub nquality: Option<f64>,
    pub rtt: Option<f64>,
    pub tx_rate: Option<f64>,
    pub tx_fail: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertKind {
    Loss,
    Flap,
    Drift,
    OneWay,
    Noise,
    /// SNR stepped down and stayed there, with a steady noise floor: antenna aim
    Aim,
    /// measures well below what the planner predicts for the path
    BelowPlan,
}

impl AlertKind {
    pub const ALL: [AlertKind; 7] = [
        AlertKind::Loss,
        AlertKind::Flap,
        AlertKind::Drift,
        AlertKind::OneWay,
        AlertKind::Noise,
        AlertKind::Aim,
        AlertKind::BelowPlan,
    ];

    /// A short stable name, for storing.
    pub fn name(self) -> &'static str {
        match self {
            AlertKind::Loss => "loss",
            AlertKind::Flap => "flap",
            AlertKind::Drift => "drift",
            AlertKind::OneWay => "one-way",
            AlertKind::Noise => "noise",
            AlertKind::Aim => "aim",
            AlertKind::BelowPlan => "below-plan",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == s)
    }

    /// What it means, in a few words.
    pub fn word(self) -> &'static str {
        match self {
            AlertKind::Loss => "link lost",
            AlertKind::Flap => "link flapping",
            AlertKind::Drift => "link slowly getting worse",
            AlertKind::OneWay => "link works one way only",
            AlertKind::Noise => "noise floor up",
            AlertKind::Aim => "antenna aim looks off",
            AlertKind::BelowPlan => "link below what the planner expects",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub kind: AlertKind,
    pub severity: f64,
    pub text: String,
    pub since: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Up,
    Down,
    Unknown,
}

/// One end's view of a link: what `node` reports about `neighbor`.
#[derive(Clone, Debug)]
pub struct Side {
    pub node: String,
    pub neighbor: String,
    /// None when this end hasn't been polled recently, so we can't say
    pub present_now: Option<bool>,
    pub last_seen: Option<i64>,
    pub latest: Option<Sample>,
    pub drops_24h: usize,
    /// absences in the last day not counted as drops, and why
    pub skipped_24h: Vec<Skipped>,
    pub down_since: Option<i64>,
    pub drift: Option<Drift>,
    pub step: Option<Step>,
    /// medians over the last few hours: SNR, our quality and the neighbor's
    pub recent_snr: Option<f64>,
    pub recent_quality: Option<f64>,
    pub recent_nquality: Option<f64>,
}

/// A slow change in a link's SNR (or quality, for tunnels and DtD), fitted with the
/// Theil-Sen estimator so a few odd readings or a rain fade don't drive it.
#[derive(Clone, Debug, PartialEq)]
pub struct Drift {
    /// how much worse it gets per week; negative means improving
    pub per_week: f64,
    /// fitted loss over the whole span
    pub total: f64,
    pub since: i64,
    /// share of point pairs agreeing with the fitted direction, 0.5 to 1
    pub agreement: f64,
}

/// Why a poll that missed a neighbor wasn't counted as the link dropping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Skipped {
    /// the node listed far fewer links than usual for that one poll
    Blank,
    /// the other end listed the link in its polls just before and after (an LQM
    /// tracker hiccup)
    OtherEndHeard,
    /// just after a firmware change or radio work at either end
    Work,
}

impl Skipped {
    pub fn text(self) -> &'static str {
        match self {
            Skipped::Blank => "the node listed only some of its links for one poll",
            Skipped::OtherEndHeard => "the other end still heard it",
            Skipped::Work => "right after a firmware change or radio work",
        }
    }
}

/// A sudden fall in SNR that has lasted: the level before and since, from hourly medians.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub at: i64,
    /// where the fitted levels start and end on a chart
    pub from: i64,
    pub before: f64,
    pub after: f64,
    /// noise floor after minus before, where both are known
    pub noise_change: Option<f64>,
}

impl Step {
    pub fn fall(&self) -> f64 {
        self.before - self.after
    }
}

/// Theil-Sen slope in units per day, with the share of point pairs whose slope has the
/// same sign as the result.
pub fn theil_sen(points: &[(i64, f64)]) -> Option<(f64, f64)> {
    let step = points.len().div_ceil(MAX_FIT_POINTS).max(1);
    let p: Vec<&(i64, f64)> = points.iter().step_by(step).collect();
    let mut slopes = Vec::with_capacity(p.len() * p.len().saturating_sub(1) / 2);
    for (i, a) in p.iter().enumerate() {
        for b in &p[i + 1..] {
            if b.0 > a.0 {
                slopes.push((b.1 - a.1) / ((b.0 - a.0) as f64 / DAY as f64));
            }
        }
    }
    let m = median(&mut slopes)?;
    let agree = slopes
        .iter()
        .filter(|x| x.signum() == m.signum() && **x != 0.0)
        .count();
    Some((m, agree as f64 / slopes.len() as f64))
}

fn drift(hours: &[Hour], rf: bool, since: i64) -> Option<Drift> {
    let pts: Vec<(i64, f64)> = hours
        .iter()
        .filter(|h| h.hour >= since)
        .filter_map(|h| Some((h.hour, if rf { h.snr? } else { h.quality? })))
        .collect();
    let (first, last) = (pts.first()?.0, pts.last()?.0);
    if pts.len() < MIN_DRIFT_HOURS || last - first < MIN_DRIFT_SPAN {
        return None;
    }
    let (slope, agreement) = theil_sen(&pts)?;
    Some(Drift {
        per_week: -slope * 7.0,
        total: -slope * (last - first) as f64 / DAY as f64,
        since: first,
        agreement,
    })
}

fn med(v: impl Iterator<Item = f64>) -> Option<f64> {
    median(&mut v.collect::<Vec<_>>())
}

/// The sharpest lasting fall in SNR since `since`: six hours against the six before
/// find the moment, then the whole stretch before has to sit clearly above the whole
/// stretch after (a slow slide fails that) and the last hours must still be down.
fn step(hours: &[Hour], since: i64, now: i64, min_db: f64) -> Option<Step> {
    let pts: Vec<(i64, f64, Option<f64>)> = hours
        .iter()
        .filter(|h| h.hour >= since.max(now - STEP_WINDOW))
        .filter_map(|h| Some((h.hour, h.snr?, h.noise)))
        .collect();
    if pts.len() < 2 * STEP_HOURS {
        return None;
    }
    // means rather than medians here, so a clean step peaks at its own hour
    let mean = |s: &[(i64, f64, Option<f64>)]| s.iter().map(|p| p.1).sum::<f64>() / s.len() as f64;
    let fall = |i: usize| mean(&pts[i - 6..i]) - mean(&pts[i..i + 6]);
    let i = (STEP_HOURS..=pts.len() - STEP_HOURS).max_by(|&x, &y| fall(x).total_cmp(&fall(y)))?;
    let (before, after) = pts.split_at(i);
    let mut b: Vec<f64> = before.iter().map(|p| p.1).collect();
    let mut a: Vec<f64> = after.iter().map(|p| p.1).collect();
    let (bm, am) = (median(&mut b)?, median(&mut a)?);
    let lasting = med(after[after.len() - 6..].iter().map(|p| p.1))?;
    if bm - am < min_db
        || history::quantile(&mut b, 0.1)? <= history::quantile(&mut a, 0.9)?
        || lasting > bm - 0.75 * (bm - am)
    {
        return None;
    }
    let noise = |s: &[(i64, f64, Option<f64>)]| {
        let v: Vec<f64> = s.iter().filter_map(|p| p.2).collect();
        (v.len() >= 12).then(|| med(v.into_iter())).flatten()
    };
    Some(Step {
        at: after[0].0,
        from: before[0].0,
        before: bm,
        after: am,
        noise_change: noise(before).zip(noise(after)).map(|(x, y)| y - x),
    })
}

/// How far one link end's SNR fell across `at`: two days before against everything since.
fn fall_at(hours: &[Hour], at: i64) -> Option<f64> {
    let snr = |r: std::ops::Range<i64>| {
        let v: Vec<f64> = hours
            .iter()
            .filter(|h| r.contains(&h.hour))
            .filter_map(|h| h.snr)
            .collect();
        (v.len() >= 6).then(|| med(v.into_iter())).flatten()
    };
    Some(snr(at - 2 * DAY..at)? - snr(at..i64::MAX)?)
}

/// The verdict on a step: Ok with the alert text when it points at antenna aim (the
/// link stepped down at both ends where both report, the noise floor held, and other
/// links at the same sites didn't move), or Err with why it doesn't. None without a step.
fn aim(
    sides: &[Side],
    hours: &BTreeMap<(String, String), Vec<Hour>>,
) -> Option<Result<String, String>> {
    // the biggest step, preferring an end that knows its noise floor on a tie
    let (s, st) = sides
        .iter()
        .filter_map(|s| Some((s, s.step.as_ref()?)))
        .max_by(|x, y| {
            x.1.fall()
                .total_cmp(&y.1.fall())
                .then(x.1.noise_change.is_some().cmp(&y.1.noise_change.is_some()))
        })?;
    Some(aim_verdict(s, st, sides, hours))
}

fn aim_verdict(
    s: &Side,
    st: &Step,
    sides: &[Side],
    hours: &BTreeMap<(String, String), Vec<Hour>>,
) -> Result<String, String> {
    if st.noise_change.is_none() {
        return Err(format!(
            "{} doesn't report its noise floor, so interference can't be ruled out",
            s.node
        ));
    }
    for o in sides {
        if let Some(n) = o.step.as_ref().and_then(|x| x.noise_change)
            && n.abs() >= STEADY_NOISE_DB
        {
            return Err(format!(
                "the noise floor at {} moved {n:+.0} dB with it, which points at interference",
                o.node
            ));
        }
    }
    if let Some(back) = hours.get(&(s.neighbor.clone(), s.node.clone()))
        && fall_at(back, st.at).is_some_and(|f| f < st.fall() / 2.0)
    {
        return Err(format!(
            "{} still hears {} as before, which points at the receive side at {} (radio, cable or connector)",
            s.neighbor, s.node, s.node
        ));
    }
    let ends = [s.node.as_str(), s.neighbor.as_str()];
    let mut compared = 0;
    for ((n, x), hs) in hours {
        if !ends.contains(&n.as_str()) || ends.contains(&x.as_str()) {
            continue;
        }
        match fall_at(hs, st.at) {
            Some(f) if f >= st.fall() / 2.0 => {
                return Err(format!(
                    "{n}'s link to {x} fell at the same time, which points at the radio or its cabling at {n}"
                ));
            }
            Some(_) => compared += 1,
            None => {}
        }
    }
    if compared == 0 {
        return Err(
            "there is no other link at these sites to compare with, so aim can't be told from a radio problem"
                .into(),
        );
    }
    Ok(format!(
        "SNR stepped down {:.0} dB at {} and has stayed there ({:.0} dB before, {:.0} since). \
         The noise floor is steady and the other links at these sites are fine, so this points \
         at antenna aim (a dish knocked or slipped on its mount) rather than interference. \
         Check the alignment at both ends, or for something new in the path.",
        st.fall(),
        short_time(st.at),
        st.before,
        st.after
    ))
}

fn recent_median(samples: &[Sample], now: i64, f: fn(&Sample) -> Option<f64>) -> Option<f64> {
    let mut v: Vec<f64> = samples
        .iter()
        .filter(|s| s.ts > now - RECENT)
        .filter_map(f)
        .collect();
    // a couple of readings say little about a sustained one-way problem
    if v.len() < 6 {
        return None;
    }
    median(&mut v)
}

#[derive(Clone, Debug)]
pub struct PairView {
    pub a: String,
    pub b: String,
    pub link_type: String,
    pub state: LinkState,
    pub sides: Vec<Side>,
    pub alerts: Vec<Alert>,
    pub last_seen: Option<i64>,
    /// why a lasting SNR step on this link isn't called an aim problem
    pub step_note: Option<String>,
}

impl PairView {
    pub fn edge(&self) -> Edge {
        edge(&self.a, &self.b)
    }

    /// The weaker of the two ends' latest SNR.
    pub fn snr(&self) -> Option<f64> {
        self.sides
            .iter()
            .filter_map(|s| s.latest.as_ref()?.snr)
            .reduce(f64::min)
    }

    pub fn quality(&self) -> Option<f64> {
        self.sides
            .iter()
            .filter_map(|s| s.latest.as_ref()?.quality)
            .reduce(f64::min)
    }

    /// Below `WEAK_SNR` or `WEAK_QUALITY` at the weaker end.
    pub fn weak(&self) -> bool {
        self.snr().is_some_and(|s| s < WEAK_SNR) || self.quality().is_some_and(|q| q < WEAK_QUALITY)
    }

    pub fn severity(&self) -> f64 {
        self.alerts.iter().map(|a| a.severity).fold(0.0, f64::max)
    }

    pub fn summary(&self, now: i64) -> String {
        let mut parts = vec![];
        match self.state {
            LinkState::Up => {
                let mut s = String::from("Up");
                if let Some(snr) = self.snr() {
                    s += &format!(", SNR {snr:.0} dB ({})", snr_word(snr));
                }
                if let Some(q) = self.quality() {
                    s += &format!(", quality {q:.0}% ({})", quality_word(q));
                }
                parts.push(s + ".");
            }
            LinkState::Down => {
                let since = self.sides.iter().filter_map(|s| s.down_since).min();
                parts.push(match since {
                    Some(t) => format!(
                        "Down since {} ({} ago).",
                        short_time(t),
                        human_duration(now - t)
                    ),
                    None => "Down.".into(),
                });
            }
            LinkState::Unknown => parts.push(match self.last_seen {
                Some(t) => format!(
                    "No fresh data; last reported {} ago.",
                    human_duration(now - t)
                ),
                None => "No data yet.".into(),
            }),
        }
        for a in &self.alerts {
            if a.kind != AlertKind::Loss {
                parts.push(a.text.clone());
            }
        }
        parts.join(" ")
    }
}

pub fn snr_word(snr: f64) -> &'static str {
    match snr {
        // AREDN's planning guidance wants 15 dB+ of fade margin for a solid link
        s if s >= 20.0 => "strong",
        s if s >= WEAK_SNR => "fine",
        s if s >= 8.0 => "weak",
        _ => "marginal",
    }
}

pub fn quality_word(q: f64) -> &'static str {
    match q {
        q if q >= 90.0 => "good",
        q if q >= 70.0 => "fair",
        _ => "poor",
    }
}

/// A successful poll of a node, with how many links it listed.
struct Poll {
    ts: i64,
    id: String,
    links: usize,
}

/// Successful polls per node, oldest first.
fn load_polls(conn: &Connection, since: i64) -> Result<BTreeMap<String, Vec<Poll>>> {
    let mut st =
        conn.prepare("SELECT node, ts, event_id FROM polls WHERE ok = 1 AND ts > ?1 ORDER BY ts")?;
    let mut ok: BTreeMap<String, Vec<Poll>> = BTreeMap::new();
    for r in st.query_map(params![since], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
    })? {
        let (n, ts, id) = r?;
        ok.entry(n).or_default().push(Poll { ts, id, links: 0 });
    }
    Ok(ok)
}

fn load_samples(conn: &Connection, since: i64) -> Result<BTreeMap<(String, String), Vec<Sample>>> {
    let mut st = conn.prepare(
        "SELECT node, neighbor, ts, event_id, link_type, snr, signal, noise, quality, nquality, rtt, tx_rate, tx_fail
         FROM link_samples WHERE ts > ?1 ORDER BY ts",
    )?;
    let mut out: BTreeMap<(String, String), Vec<Sample>> = BTreeMap::new();
    for r in st.query_map(params![since], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            Sample {
                ts: r.get(2)?,
                event_id: r.get(3)?,
                link_type: r.get(4)?,
                snr: r.get(5)?,
                signal: r.get(6)?,
                noise: r.get(7)?,
                quality: r.get(8)?,
                nquality: r.get(9)?,
                rtt: r.get(10)?,
                tx_rate: r.get(11)?,
                tx_fail: r.get(12)?,
            },
        ))
    })? {
        let (k, s) = r?;
        out.entry(k).or_default().push(s);
    }
    Ok(out)
}

/// A poll in which at least two links vanished at once, more than half of what the poll
/// before listed, and which the next poll contradicts by listing them again. That is the
/// node's link table or our poll coming back partial, not every link dropping. The latest
/// poll can't be contradicted yet, so it gets the benefit of the doubt until the next.
fn blank(polls: &[Poll], i: usize) -> bool {
    let Some(prev) = i.checked_sub(1).and_then(|j| polls.get(j)) else {
        return false;
    };
    let cur = polls[i].links;
    prev.links >= cur + 2
        && cur * 2 <= prev.links
        && polls.get(i + 1).is_none_or(|n| n.links + 1 >= prev.links)
}

/// Whether the other end listed this link in its last poll before `t` and in its first
/// poll at least `TRACKER_LAG` after it, both within `gap`. Then a poll here that missed
/// the link is a hiccup in this node's LQM tracker, not the link dropping. A far poll
/// sooner after `t` proves nothing: its tracker keeps a dead neighbor for that long too.
fn heard_by_far_end(e: &End, t: i64, gap: i64) -> bool {
    let listed: HashSet<&str> = e.reverse.iter().map(|s| s.event_id.as_str()).collect();
    let i = e.far_polls.partition_point(|f| f.ts < t);
    let before = i.checked_sub(1).and_then(|j| e.far_polls.get(j));
    let after = e.far_polls[i..].iter().find(|f| f.ts >= t + TRACKER_LAG);
    let ok = |f: Option<&Poll>, within: i64| {
        f.is_some_and(|f| (f.ts - t).abs() <= within && listed.contains(f.id.as_str()))
    };
    ok(before, gap) && ok(after, gap + TRACKER_LAG)
}

/// What one end of a link is measured against: the end's own data, the other end's
/// samples, and the times of radio changes at either end.
struct End<'a> {
    node: &'a str,
    nb: &'a str,
    samples: &'a [Sample],
    reverse: &'a [Sample],
    polls: &'a [Poll],
    /// the other end's polls
    far_polls: &'a [Poll],
    hours: &'a [Hour],
    changes: &'a [i64],
}

fn side(e: &End, cfg: &LinksCfg, now: i64, fresh_ms: i64) -> Side {
    let present: HashSet<&str> = e.samples.iter().map(|s| s.event_id.as_str()).collect();
    let mut flags: Vec<(i64, bool)> = vec![];
    let mut drops = 0;
    let mut skipped = vec![];
    for (i, p) in e.polls.iter().enumerate() {
        let here = present.contains(p.id.as_str());
        let dropped = flags.last().is_some_and(|f| f.1) && !here;
        let recent = p.ts > now - DAY;
        if dropped && blank(e.polls, i) {
            if recent {
                skipped.push(Skipped::Blank);
            }
            continue;
        }
        if dropped && recent {
            let gap = i.checked_sub(1).map_or(0, |j| p.ts - e.polls[j].ts);
            if heard_by_far_end(e, p.ts, gap) {
                skipped.push(Skipped::OtherEndHeard);
            } else if e.changes.iter().any(|c| (0..WORK_MS).contains(&(p.ts - c))) {
                skipped.push(Skipped::Work);
            } else {
                drops += 1;
            }
        }
        flags.push((p.ts, here));
    }
    // one poll without the neighbor is often just a hiccup in the node's link table, so a
    // link only counts as lost after two in a row
    let absent_run = flags.iter().rev().take_while(|(_, p)| !p).count();
    let present_now = flags
        .last()
        .filter(|(ts, _)| *ts >= now - fresh_ms)
        .map(|(_, p)| *p || absent_run < 2);

    let down_since = if present_now == Some(false) {
        let last_present = flags.iter().rposition(|(_, p)| *p);
        match last_present {
            Some(i) => flags.get(i + 1).map(|(ts, _)| *ts),
            None => flags.first().map(|(ts, _)| *ts),
        }
    } else {
        None
    };

    let rf = e
        .samples
        .last()
        .and_then(|s| s.link_type.as_deref())
        .is_none_or(|t| t == "RF");
    let since = e
        .changes
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        .max(now - DRIFT_WINDOW);
    Side {
        node: e.node.to_string(),
        neighbor: e.nb.to_string(),
        present_now,
        last_seen: e.samples.last().map(|s| s.ts),
        latest: e.samples.last().cloned(),
        drops_24h: drops,
        skipped_24h: skipped,
        down_since,
        drift: drift(e.hours, rf, since),
        step: rf
            .then(|| step(e.hours, since, now, cfg.aim_step_db))
            .flatten(),
        recent_snr: recent_median(e.samples, now, |s| s.snr),
        recent_quality: recent_median(e.samples, now, |s| s.quality),
        recent_nquality: recent_median(e.samples, now, |s| s.nquality),
    }
}

/// A one-way link: one end hears the other much better than the other way round.
fn one_way(sides: &[Side], cfg: &LinksCfg) -> Option<String> {
    for s in sides {
        if let (Some(q), Some(nq)) = (s.recent_quality, s.recent_nquality)
            && (q - nq).abs() >= cfg.one_way_points
        {
            return Some(format!(
                "One-way: {} receives {} at {q:.0}% quality, but {} receives {} at {nq:.0}%.",
                s.node, s.neighbor, s.neighbor, s.node
            ));
        }
    }
    if let [a, b] = sides
        && let (Some(x), Some(y)) = (a.recent_snr, b.recent_snr)
        && (x - y).abs() >= cfg.one_way_db
    {
        return Some(format!(
            "One-way: {} hears {} at {x:.0} dB SNR, but {} hears {} at {y:.0} dB.",
            a.node, a.neighbor, b.node, b.neighbor
        ));
    }
    None
}

/// When either end had a radio change, from firmware changes and notes.
fn changes(conn: &Connection, a: &str, b: &str) -> Result<Vec<i64>> {
    Ok(history::marks(conn, a, b)?
        .into_iter()
        .filter(|m| m.change)
        .map(|m| m.ts)
        .collect())
}

/// Analyze every link seen in the last week.
pub fn analyze(
    conn: &Connection,
    now: i64,
    fresh_ms: i64,
    cfg: &LinksCfg,
) -> Result<Vec<PairView>> {
    let since = now - HISTORY_MS;
    let mut polls = load_polls(conn, since)?;
    let samples = load_samples(conn, since)?;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for s in samples.values().flatten() {
        *counts.entry(s.event_id.as_str()).or_default() += 1;
    }
    for p in polls.values_mut().flatten() {
        p.links = counts.get(p.id.as_str()).copied().unwrap_or(0);
    }
    let hours = history::all_hours(conn, now - DRIFT_WINDOW, now)?;

    let mut pairs: BTreeMap<Edge, Vec<Side>> = BTreeMap::new();
    for ((node, nb), ss) in &samples {
        let key = (nb.clone(), node.clone());
        let e = End {
            node,
            nb,
            samples: ss,
            reverse: samples.get(&key).map_or(&[], Vec::as_slice),
            polls: polls.get(node).map_or(&[], Vec::as_slice),
            far_polls: polls.get(nb).map_or(&[], Vec::as_slice),
            hours: hours
                .get(&(node.clone(), nb.clone()))
                .map_or(&[], Vec::as_slice),
            changes: &changes(conn, node, nb)?,
        };
        pairs
            .entry(edge(node, nb))
            .or_default()
            .push(side(&e, cfg, now, fresh_ms));
    }

    let mut out = vec![];
    for ((a, b), sides) in pairs {
        let link_type = sides
            .iter()
            .filter_map(|s| s.latest.as_ref()?.link_type.clone())
            .next()
            .unwrap_or_else(|| "RF".into());
        let state = if sides.iter().any(|s| s.present_now == Some(true)) {
            LinkState::Up
        } else if sides.iter().any(|s| s.present_now == Some(false)) {
            LinkState::Down
        } else {
            LinkState::Unknown
        };
        let mut alerts = vec![];
        if state == LinkState::Down {
            let since = sides.iter().filter_map(|s| s.down_since).min();
            alerts.push(Alert {
                kind: AlertKind::Loss,
                severity: 3.0,
                text: match since {
                    Some(t) => format!("Link lost at {}.", short_time(t)),
                    None => "Link lost.".into(),
                },
                since,
            });
        }
        let drops = sides.iter().map(|s| s.drops_24h).max().unwrap_or(0);
        if drops >= cfg.flap_count {
            alerts.push(Alert {
                kind: AlertKind::Flap,
                severity: 2.0 + (drops as f64 / 10.0).min(1.0),
                text: format!("Dropped {drops} times in the last 24 h (flapping)."),
                since: None,
            });
        }
        // a step explains any drift the fit shows, so only one of the two is reported
        let (aimed, step_note) = match (link_type == "RF").then(|| aim(&sides, &hours)).flatten() {
            Some(Ok(text)) => (Some(text), None),
            Some(Err(why)) => (None, Some(why)),
            None => (None, None),
        };
        let threshold = if link_type == "RF" {
            cfg.drift_db
        } else {
            15.0
        };
        if let Some(text) = aimed {
            alerts.push(Alert {
                kind: AlertKind::Aim,
                severity: 2.4,
                since: sides
                    .iter()
                    .filter_map(|s| s.step.as_ref())
                    .map(|s| s.at)
                    .min(),
                text,
            });
        } else if let Some(d) = sides
            .iter()
            .filter_map(|s| s.drift.as_ref())
            .filter(|d| d.agreement >= MIN_DRIFT_AGREEMENT)
            .max_by(|x, y| x.per_week.total_cmp(&y.per_week))
            && d.per_week >= threshold
        {
            let unit = if link_type == "RF" {
                "dB SNR"
            } else {
                "points of quality"
            };
            alerts.push(Alert {
                kind: AlertKind::Drift,
                // grows with the drift but stays below a lost link (3.0)
                severity: (1.5 + d.per_week / (threshold * 2.0)).min(2.9),
                text: format!(
                    "Slowly getting worse: losing about {:.1} {unit} a week, {:.0} in all since {}. \
                     Check dish alignment, connectors and water ingress.",
                    d.per_week,
                    d.total,
                    short_time(d.since)
                ),
                since: Some(d.since),
            });
        }
        if state == LinkState::Up
            && let Some(text) = one_way(&sides, cfg)
        {
            alerts.push(Alert {
                kind: AlertKind::OneWay,
                severity: 2.0,
                text: format!(
                    "{text} That usually means a weak transmitter, cable or connector at one end, \
                     or interference near the end that hears worse."
                ),
                since: None,
            });
        }
        let last_seen = sides.iter().filter_map(|s| s.last_seen).max();
        out.push(PairView {
            a,
            b,
            link_type,
            state,
            sides,
            alerts,
            last_seen,
            step_note,
        });
    }
    Ok(out)
}

/// Every site heard from in the last week: polled directly or reported as a neighbor.
pub fn known_sites(conn: &Connection, now: i64) -> Result<BTreeSet<String>> {
    let mut st = conn.prepare(
        "SELECT node FROM polls WHERE ts > ?1 UNION SELECT neighbor FROM link_samples WHERE ts > ?1",
    )?;
    let rows = st.query_map(params![now - 7 * DAY], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub struct Topology {
    pub graph: Graph,
    pub anchor: Option<String>,
    /// sites already cut off from the anchor
    pub cut_now: BTreeSet<String>,
    /// site -> what it cuts off if it fails
    pub aps: Vec<(String, BTreeSet<String>)>,
    /// link -> what it cuts off if it fails
    pub bridges: Vec<(Edge, BTreeSet<String>)>,
}

/// Build the live graph from links that are up and find the single points of failure.
pub fn topology(pairs: &[PairView], sites: &BTreeSet<String>, anchor: Option<&str>) -> Topology {
    let mut g = Graph::default();
    for s in sites {
        g.add_node(s);
    }
    // links with no fresh data are assumed to still be there: "we don't know" must not
    // turn into "it's broken" in a list of single points of failure
    for p in pairs.iter().filter(|p| p.state != LinkState::Down) {
        g.add_edge(&p.a, &p.b);
    }
    let (aps, bridges) = g.cut_points();
    let no_nodes: BTreeSet<String> = BTreeSet::new();
    let no_edges: BTreeSet<Edge> = BTreeSet::new();
    // sites already cut off aren't casualties of the next failure
    let already = g.what_if(&no_nodes, &no_edges, anchor);
    let aps = aps
        .into_iter()
        .map(|x| {
            // if the failed site is where we're looking from, judge from the biggest side
            let from = anchor.filter(|a| *a != x);
            let cut: BTreeSet<String> = g
                .what_if(&[x.clone()].into_iter().collect(), &no_edges, from)
                .difference(&already)
                .cloned()
                .collect();
            (x, cut)
        })
        .filter(|(_, cut)| !cut.is_empty())
        .collect();
    let bridges = bridges
        .into_iter()
        .map(|e| {
            let cut: BTreeSet<String> = g
                .what_if(&no_nodes, &[e.clone()].into_iter().collect(), anchor)
                .difference(&already)
                .cloned()
                .collect();
            (e, cut)
        })
        .filter(|(_, cut)| !cut.is_empty())
        .collect();
    Topology {
        graph: g,
        anchor: anchor.map(str::to_string),
        cut_now: already,
        aps,
        bridges,
    }
}

/// A problem at one site rather than on one link.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeAlert {
    pub node: String,
    pub alert: Alert,
}

/// Noise floor against the same hour of day over the past month: a sustained rise is
/// usually new interference near the site.
pub fn node_alerts(conn: &Connection, now: i64, cfg: &LinksCfg) -> Result<Vec<NodeAlert>> {
    let hours = history::all_hours(conn, now - DRIFT_WINDOW, now)?;
    let hod = |t: i64| t.rem_euclid(DAY) / HOUR;
    let mut by_hour: BTreeMap<(&str, i64), Vec<f64>> = BTreeMap::new();
    for ((node, _), hs) in &hours {
        for h in hs.iter().filter(|h| h.hour < now - DAY) {
            if let Some(n) = h.noise {
                by_hour.entry((node, hod(h.hour))).or_default().push(n);
            }
        }
    }
    // the usual noise at each site for each hour of the day, where there's enough history
    let base: BTreeMap<(&str, i64), f64> = by_hour
        .into_iter()
        .filter(|(_, v)| v.len() >= 5)
        .filter_map(|(k, mut v)| Some((k, median(&mut v)?)))
        .collect();
    let mut st = conn.prepare(
        "SELECT node, ts, noise FROM link_samples WHERE ts > ?1 AND noise IS NOT NULL
         AND coalesce(link_type, 'RF') = 'RF'",
    )?;
    // each recent reading against the usual for its own hour
    let mut rises: BTreeMap<String, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for r in st.query_map(params![now - 2 * HOUR], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, f64>(2)?,
        ))
    })? {
        let (node, ts, n) = r?;
        if let Some(&b) = base.get(&(node.as_str(), hod(ts))) {
            let e = rises.entry(node).or_default();
            e.0.push(n - b);
            e.1.push(n);
        }
    }
    let mut out = vec![];
    for (node, (mut rise, mut now_vals)) in rises {
        if rise.len() < 4 {
            continue;
        }
        let (Some(r), Some(c)) = (median(&mut rise), median(&mut now_vals)) else {
            continue;
        };
        if r >= cfg.noise_rise_db {
            out.push(NodeAlert {
                alert: Alert {
                    kind: AlertKind::Noise,
                    severity: (1.5 + r / 10.0).min(2.5),
                    text: format!(
                        "Noise floor at {node} is {r:.0} dB above normal for this time of day \
                         ({c:.0} dBm, usually {:.0}). Look for new interference near the site.",
                        c - r
                    ),
                    since: None,
                },
                node,
            });
        }
    }
    Ok(out)
}

/// One thing to fix, scored as risk times the sites it affects.
#[derive(Clone, Debug)]
pub struct WorkItem {
    pub score: f64,
    /// worst alert severity, 0 to 3
    pub risk: f64,
    /// sites cut off if this fails (or restored if it's fixed)
    pub affected: BTreeSet<String>,
    pub link: Option<Edge>,
    pub reasons: Vec<String>,
    /// set when the link is weak and is the only path to these sites
    pub only_path_to: BTreeSet<String>,
}

/// A trip to one site and everything worth doing there, highest total score first.
#[derive(Clone, Debug)]
pub struct Visit {
    pub site: String,
    pub score: f64,
    pub items: Vec<WorkItem>,
}

/// Which end of a link to go to: the end it cuts off (or would bring back), else the end
/// that hears worse.
fn visit_site(p: &PairView, affected: &BTreeSet<String>) -> String {
    if affected.contains(&p.b) {
        return p.b.clone();
    }
    if affected.contains(&p.a) {
        return p.a.clone();
    }
    p.sides
        .iter()
        .filter_map(|s| Some((s.node.clone(), s.latest.as_ref()?.snr?)))
        .min_by(|x, y| x.1.total_cmp(&y.1))
        .map_or_else(|| p.b.clone(), |(n, _)| n)
}

/// The next-workday list: anything alerting, plus weak links that are the only path to
/// somewhere, scored as risk times (1 + sites affected) and grouped by site.
pub fn workday(pairs: &[PairView], nodes: &[NodeAlert], topo: &Topology) -> Vec<Visit> {
    let bridge_cuts: BTreeMap<&Edge, &BTreeSet<String>> =
        topo.bridges.iter().map(|(e, c)| (e, c)).collect();
    let ap_cuts: BTreeMap<&String, &BTreeSet<String>> =
        topo.aps.iter().map(|(n, c)| (n, c)).collect();
    let mut by_site: BTreeMap<String, Vec<WorkItem>> = BTreeMap::new();
    for p in pairs {
        let mut risk = p.severity();
        let reasons: Vec<String> = p.alerts.iter().map(|a| a.text.clone()).collect();
        let cut = bridge_cuts.get(&p.edge()).copied();
        let mut only_path_to = BTreeSet::new();
        if let Some(c) = cut
            && p.weak()
        {
            risk = risk.max(1.5);
            only_path_to = c.clone();
        }
        if risk <= 0.0 {
            continue;
        }
        let affected = match p.state {
            LinkState::Down => restored_by(topo, &p.a, &p.b),
            _ => cut.cloned().unwrap_or_default(),
        };
        by_site
            .entry(visit_site(p, &affected))
            .or_default()
            .push(WorkItem {
                score: risk * (1 + affected.len()) as f64,
                risk,
                affected,
                link: Some(p.edge()),
                reasons,
                only_path_to,
            });
    }
    for n in nodes {
        let affected = ap_cuts
            .get(&n.node)
            .map(|c| (*c).clone())
            .unwrap_or_default();
        by_site.entry(n.node.clone()).or_default().push(WorkItem {
            score: n.alert.severity * (1 + affected.len()) as f64,
            risk: n.alert.severity,
            affected,
            link: None,
            reasons: vec![n.alert.text.clone()],
            only_path_to: BTreeSet::new(),
        });
    }
    let mut visits: Vec<Visit> = by_site
        .into_iter()
        .map(|(site, mut items)| {
            items.sort_by(|a, b| b.score.total_cmp(&a.score));
            Visit {
                score: items.iter().map(|i| i.score).sum(),
                site,
                items,
            }
        })
        .collect();
    visits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.site.cmp(&b.site))
    });
    visits
}

/// Sites now cut off that would be reachable again if the a-b link came back.
pub fn restored_by(topo: &Topology, a: &str, b: &str) -> BTreeSet<String> {
    let mut g = topo.graph.clone();
    g.add_edge(a, b);
    let after = g.what_if(&BTreeSet::new(), &BTreeSet::new(), topo.anchor.as_deref());
    topo.cut_now.difference(&after).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::mesh::Mesh;
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    const MIN: i64 = 60_000;
    const NOW: i64 = 1_800_000_000_000;

    fn log() -> Fake {
        Fake::new(vec![Box::new(Mesh), Box::new(crate::history::Notes)])
    }

    /// A poll of `node` with full link fields.
    /// A poll of a, reporting b with (snr, quality, neighbor's quality, noise).
    fn poll_full(l: &mut Fake, ts: i64, (snr, q, nq, noise): (f64, f64, f64, f64)) {
        l.add(
            "node.poll",
            json!({"node": "a", "ok": true, "links": [
                {"neighbor": "b", "type": "RF", "snr": snr, "quality": q, "nquality": nq, "noise": noise}
            ]}),
            ts,
        );
    }

    /// A poll of `node` at `ts` reporting `links` as (neighbor, snr).
    fn poll(l: &mut Fake, ts: i64, node: &str, links: &[(&str, f64)]) {
        let links: Vec<_> = links
            .iter()
            .map(|(n, snr)| json!({"neighbor": n, "type": "RF", "snr": snr, "quality": 95.0}))
            .collect();
        l.add(
            "node.poll",
            json!({"node": node, "ok": true, "links": links}),
            ts,
        );
    }

    fn analyze_now(l: &Fake) -> Vec<PairView> {
        analyze(&l.conn, NOW, 20 * MIN, &LinksCfg::default()).unwrap()
    }

    #[test]
    fn steady_link_is_up_with_no_alerts() {
        let mut l = log();
        for i in 0..12 {
            poll(&mut l, NOW - (12 - i) * 5 * MIN, "a", &[("b", 25.0)]);
        }
        let p = analyze_now(&l);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].state, LinkState::Up);
        assert!(p[0].alerts.is_empty());
        assert!(p[0].summary(NOW).starts_with("Up, SNR 25 dB (strong)"));
    }

    #[test]
    fn lost_link_reports_when() {
        let mut l = log();
        poll(&mut l, NOW - 30 * MIN, "a", &[("b", 25.0)]);
        poll(&mut l, NOW - 25 * MIN, "a", &[("b", 25.0)]);
        poll(&mut l, NOW - 20 * MIN, "a", &[]);
        poll(&mut l, NOW - 5 * MIN, "a", &[]);
        let p = analyze_now(&l);
        assert_eq!(p[0].state, LinkState::Down);
        assert_eq!(p[0].sides[0].down_since, Some(NOW - 20 * MIN));
        assert_eq!(p[0].alerts[0].kind, AlertKind::Loss);
    }

    #[test]
    fn stale_node_is_unknown_not_down() {
        let mut l = log();
        poll(&mut l, NOW - 3 * 60 * MIN, "a", &[("b", 25.0)]);
        let p = analyze_now(&l);
        assert_eq!(p[0].state, LinkState::Unknown);
        assert!(p[0].alerts.is_empty());
    }

    #[test]
    fn flapping_is_flagged() {
        let mut l = log();
        for i in 0..24 {
            let links: &[(&str, f64)] = if i % 2 == 0 { &[("b", 20.0)] } else { &[] };
            poll(&mut l, NOW - (24 - i) * 10 * MIN, "a", links);
        }
        poll(&mut l, NOW - MIN, "a", &[("b", 20.0)]);
        let p = analyze_now(&l);
        assert!(
            p[0].alerts.iter().any(|a| a.kind == AlertKind::Flap),
            "{:?}",
            p[0].alerts
        );
        assert_eq!(p[0].state, LinkState::Up);
    }

    #[test]
    fn slow_drift_is_caught_and_steady_is_not() {
        let mut l = log();
        // a week of 30-minute samples: a-b loses 5 dB, a-c holds steady with a daily wiggle
        let mut t = NOW - 8 * DAY;
        while t <= NOW {
            let age_days = (NOW - t) as f64 / DAY as f64;
            let wiggle = ((t / (60 * MIN)) % 24) as f64 / 12.0 - 1.0;
            poll(
                &mut l,
                t,
                "a",
                &[
                    ("b", 25.0 - 5.0 * (8.0 - age_days) / 7.0),
                    ("c", 22.0 + wiggle),
                ],
            );
            t += 30 * MIN;
        }
        let p = analyze_now(&l);
        let ab = p.iter().find(|p| p.b == "b").unwrap();
        let ac = p.iter().find(|p| p.b == "c").unwrap();
        assert!(
            ab.alerts.iter().any(|a| a.kind == AlertKind::Drift),
            "{:?}",
            ab.alerts
        );
        assert!(ac.alerts.is_empty(), "{:?}", ac.alerts);
    }

    #[test]
    fn theil_sen_shrugs_off_outliers() {
        let mut pts: Vec<(i64, f64)> = (0..100)
            .map(|i| (i * HOUR, 20.0 - i as f64 / 24.0))
            .collect();
        // a rain fade and a bad reading
        pts[40].1 = -5.0;
        pts[41].1 = -3.0;
        pts[70].1 = 60.0;
        let (slope, agree) = theil_sen(&pts).unwrap();
        assert!((slope + 1.0).abs() < 0.01, "{slope}");
        assert!(agree > 0.9);
    }

    #[test]
    fn drift_restarts_after_a_radio_change() {
        let mut l = log();
        // falls 1 dB a day for six days, then the connector is replaced and it holds steady
        let mut t = NOW - 10 * DAY;
        while t <= NOW {
            let age = (NOW - t) as f64 / DAY as f64;
            let snr = if age > 4.0 { 20.0 - (10.0 - age) } else { 14.0 };
            poll(&mut l, t, "a", &[("b", snr)]);
            t += 30 * MIN;
        }
        let drifting = |l: &Fake| {
            analyze_now(l)[0]
                .alerts
                .iter()
                .any(|a| a.kind == AlertKind::Drift)
        };
        assert!(
            drifting(&l),
            "without knowing about the change it looks like drift"
        );
        l.add(
            "link.note",
            json!({"node": "a", "other": "b", "text": "realigned dish", "change": true}),
            NOW - 4 * DAY,
        );
        assert!(!drifting(&l));
    }

    #[test]
    fn one_way_links_are_flagged() {
        let mut l = log();
        for i in 0..10 {
            poll_full(&mut l, NOW - (10 - i) * 5 * MIN, (25.0, 95.0, 40.0, -95.0));
        }
        let p = analyze_now(&l);
        let a = p[0]
            .alerts
            .iter()
            .find(|a| a.kind == AlertKind::OneWay)
            .unwrap();
        assert!(
            a.text.contains("95%") && a.text.contains("40%"),
            "{}",
            a.text
        );
        let mut l = log();
        for i in 0..10 {
            poll_full(&mut l, NOW - (10 - i) * 5 * MIN, (25.0, 95.0, 90.0, -95.0));
        }
        assert!(analyze_now(&l)[0].alerts.is_empty());
    }

    #[test]
    fn noise_rise_against_the_same_hour_is_flagged() {
        let mut l = log();
        let mut t = NOW - 10 * DAY;
        while t < NOW - 2 * HOUR {
            poll_full(&mut l, t, (25.0, 95.0, 95.0, -95.0));
            t += HOUR;
        }
        let cfg = LinksCfg::default();
        assert!(node_alerts(&l.conn, NOW, &cfg).unwrap().is_empty());
        for i in 0..6 {
            poll_full(&mut l, NOW - (6 - i) * 15 * MIN, (15.0, 95.0, 95.0, -85.0));
        }
        let n = node_alerts(&l.conn, NOW, &cfg).unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].node, "a");
        assert!(
            n[0].alert.text.contains("10 dB above"),
            "{}",
            n[0].alert.text
        );
    }

    #[test]
    fn rollups_keep_history_after_raw_samples_go() {
        let mut l = log();
        let mut t = NOW - 40 * DAY;
        while t <= NOW {
            poll(&mut l, t, "a", &[("b", 20.0)]);
            t += 30 * MIN;
        }
        crate::history::rollup(&l.conn, NOW).unwrap();
        let raw: i64 = l
            .conn
            .query_row(
                "SELECT count(*) FROM link_samples WHERE ts < ?1",
                [NOW - 30 * DAY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, 0);
        let hours = crate::history::hours(&l.conn, "a", "b", NOW - 40 * DAY, NOW).unwrap();
        // 40 days of hours, the oldest rolled up before the raw samples went
        assert!(hours.len() >= 40 * 24, "{}", hours.len());
        assert!(
            hours.windows(2).all(|w| w[0].hour < w[1].hour),
            "no hour twice"
        );
        crate::history::rollup(&l.conn, NOW + HOUR).unwrap();
        let again = crate::history::hours(&l.conn, "a", "b", NOW - 40 * DAY, NOW).unwrap();
        assert_eq!(again.len(), hours.len());
        assert_eq!(hours[0].snr, Some(20.0));
        // a site that was cut off syncs three-day-old samples after those hours were rolled
        for i in 0..4 {
            poll(&mut l, NOW - 3 * DAY + i * 10 * MIN, "c", &[("d", 12.0)]);
        }
        crate::history::rollup(&l.conn, NOW + 2 * HOUR).unwrap();
        let late = crate::history::hours(&l.conn, "c", "d", NOW - 40 * DAY, NOW).unwrap();
        assert_eq!(late.len(), 1);
        assert_eq!((late[0].n, late[0].snr), (4, Some(12.0)));
        let ab = crate::history::hours(&l.conn, "a", "b", NOW - 40 * DAY, NOW).unwrap();
        assert_eq!(ab.len(), hours.len(), "re-rolling that hour kept a-b's");
    }

    #[test]
    fn one_missed_poll_is_not_a_lost_link() {
        let mut l = log();
        poll(&mut l, NOW - 15 * MIN, "a", &[("b", 25.0)]);
        poll(&mut l, NOW - 10 * MIN, "a", &[("b", 25.0)]);
        poll(&mut l, NOW - 5 * MIN, "a", &[]);
        let p = analyze_now(&l);
        assert_eq!(p[0].state, LinkState::Up);
        assert!(p[0].alerts.is_empty());
    }

    #[test]
    fn noise_is_judged_against_each_hour_not_just_this_one() {
        let mut l = log();
        // a site that is always 7 dB noisier from 16:00 to 02:00 UTC; nothing has changed
        let noise = |t: i64| {
            if (16..24).contains(&(t.rem_euclid(DAY) / HOUR)) || t.rem_euclid(DAY) / HOUR < 2 {
                -88.0
            } else {
                -95.0
            }
        };
        let mut t = NOW - 10 * DAY;
        while t <= NOW {
            poll_full(&mut l, t, (25.0, 95.0, 95.0, noise(t)));
            t += 15 * MIN;
        }
        // NOW is 08:00 UTC; look at 02:30 the next day, just after the noisy hours end
        let at = NOW + 18 * HOUR + 30 * MIN;
        while t <= at {
            poll_full(&mut l, t, (25.0, 95.0, 95.0, noise(t)));
            t += 15 * MIN;
        }
        assert!(
            node_alerts(&l.conn, at, &LinksCfg::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn already_cut_off_sites_are_not_counted_again() {
        let mut l = log();
        // a - b - c, and z which used to link to c but is down now
        poll(&mut l, NOW - 30 * MIN, "c", &[("b", 25.0), ("z", 25.0)]);
        poll(&mut l, NOW - 5 * MIN, "a", &[("b", 25.0)]);
        poll(&mut l, NOW - 5 * MIN, "b", &[("a", 25.0), ("c", 25.0)]);
        poll(&mut l, NOW - 10 * MIN, "c", &[("b", 25.0)]);
        poll(&mut l, NOW - 5 * MIN, "c", &[("b", 25.0)]);
        let pairs = analyze_now(&l);
        let sites = known_sites(&l.conn, NOW).unwrap();
        let topo = topology(&pairs, &sites, Some("a"));
        let (_, cut) = topo.aps.iter().find(|(x, _)| x == "b").unwrap();
        assert_eq!(
            cut.iter().cloned().collect::<Vec<_>>(),
            vec!["c".to_string()],
            "z was already gone"
        );
    }

    #[test]
    fn workday_ranks_and_flags_weak_bridges() {
        let mut l = log();
        poll(&mut l, NOW - 30 * MIN, "a", &[("b", 25.0), ("c", 10.0)]);
        poll(&mut l, NOW - 30 * MIN, "b", &[("a", 25.0), ("d", 25.0)]);
        poll(&mut l, NOW - 5 * MIN, "a", &[("b", 25.0), ("c", 10.0)]);
        poll(&mut l, NOW - 10 * MIN, "b", &[("a", 25.0)]);
        poll(&mut l, NOW - 5 * MIN, "b", &[("a", 25.0)]);
        let pairs = analyze_now(&l);
        let sites = known_sites(&l.conn, NOW).unwrap();
        let topo = topology(&pairs, &sites, Some("a"));
        let w = workday(&pairs, &[], &topo);
        // the lost link cuts d off, so it scores 3 x (1 + 1); the weak bridge to c 1.5 x 2
        assert_eq!(w[0].site, "d", "go where the lost link is");
        assert_eq!(w[0].items[0].link, Some(edge("b", "d")));
        assert_eq!(w[0].items[0].score, 6.0);
        let c = w.iter().find(|v| v.site == "c").unwrap();
        assert!(c.items[0].only_path_to.contains("c"));
        assert_eq!(c.score, 3.0);
        assert!(
            topo.aps
                .iter()
                .any(|(x, cut)| x == "a" && cut.contains("c"))
        );
    }

    /// A poll of `node` at `ts` reporting (neighbor, snr, noise), with a firmware version.
    fn poll_n(l: &mut Fake, ts: i64, node: &str, fw: &str, links: &[(&str, f64, f64)]) {
        let links: Vec<_> = links
            .iter()
            .map(|(n, snr, noise)| {
                json!({"neighbor": n, "type": "RF", "snr": snr, "noise": noise, "quality": 95.0})
            })
            .collect();
        l.add(
            "node.poll",
            json!({"node": node, "ok": true, "firmware": fw, "links": links}),
            ts,
        );
    }

    fn flaps(p: &[PairView], b: &str) -> (usize, Vec<Skipped>) {
        let s = &p.iter().find(|p| p.b == b).unwrap().sides[0];
        (s.drops_24h, s.skipped_24h.clone())
    }

    #[test]
    fn drops_from_our_own_polling_are_left_out() {
        let mut l = log();
        let all = [("b", 20.0), ("c", 20.0), ("d", 20.0)];
        for i in 0..36 {
            let t = NOW - (36 - i) * 10 * MIN;
            // every fourth poll comes back with almost nothing in it
            let links: &[(&str, f64)] = if i % 4 == 1 { &all[..1] } else { &all };
            poll(&mut l, t, "a", links);
        }
        poll(&mut l, NOW - MIN, "a", &all);
        let p = analyze_now(&l);
        let (n, skipped) = flaps(&p, "c");
        assert_eq!(n, 0);
        assert_eq!(skipped.len(), 9);
        assert!(skipped.iter().all(|k| *k == Skipped::Blank));
        assert!(
            p.iter()
                .all(|p| !p.alerts.iter().any(|a| a.kind == AlertKind::Flap))
        );
        assert_eq!(flaps(&p, "b").0, 0, "b was in every poll");
    }

    #[test]
    fn a_link_the_other_end_still_heard_or_dropped_during_work_is_not_flapping() {
        let mut l = log();
        for i in 0..24 {
            let t = NOW - (24 - i) * 10 * MIN;
            let up = i % 2 == 0;
            let links: &[(&str, f64)] = if up {
                &[("b", 20.0), ("c", 20.0)]
            } else {
                &[("c", 20.0)]
            };
            poll(&mut l, t, "a", links);
            // b keeps hearing a the whole time: a's tracker for b is what blinks
            poll(&mut l, t + 20_000, "b", &[("a", 20.0)]);
        }
        let p = analyze_now(&l);
        assert_eq!(
            flaps(&p, "b").0,
            1,
            "the last drop counts until a later poll of b says otherwise"
        );
        poll(&mut l, NOW - 30_000, "b", &[("a", 20.0)]);
        let p = analyze_now(&l);
        let (n, skipped) = flaps(&p, "b");
        assert_eq!(n, 0);
        assert!(skipped.iter().all(|k| *k == Skipped::OtherEndHeard));
        assert!(skipped.len() >= 10, "{skipped:?}");

        let mut l = log();
        for i in 0..24 {
            let t = NOW - (24 - i) * 2 * MIN;
            let fw = if i < 10 { "3.25.8.0" } else { "4.26.7.0" };
            let links: &[(&str, f64, f64)] = if i % 2 == 0 {
                &[("b", 20.0, -95.0), ("c", 20.0, -95.0)]
            } else {
                &[("c", 20.0, -95.0)]
            };
            poll_n(&mut l, t, "a", fw, links);
        }
        crate::history::rollup(&l.conn, NOW).unwrap();
        let (n, skipped) = flaps(&analyze_now(&l), "b");
        assert!(skipped.contains(&Skipped::Work), "{skipped:?}");
        assert_eq!(n, 5, "drops before the upgrade still count");
    }

    /// How a stepped link looks; see `stepped_with`.
    struct Steps {
        fall: f64,
        noise_rise: f64,
        /// a's link to c, and how far it falls with the step; None for no such link
        c_fall: Option<f64>,
        /// whether b hears a worse too
        both_ends: bool,
        /// back to the old level a day ago
        recovers: bool,
    }

    const STEP: Steps = Steps {
        fall: 8.0,
        noise_rise: 0.0,
        c_fall: Some(0.0),
        both_ends: true,
        recovers: false,
    };

    /// Ten days of half-hourly polls at a and b: a-b steps down three days ago.
    fn stepped_with(k: Steps) -> Vec<PairView> {
        let mut l = log();
        let at = NOW - 3 * DAY;
        let mut t = NOW - 10 * DAY;
        while t <= NOW {
            let after = t >= at && !(k.recovers && t >= NOW - DAY);
            let jig = if (t / (30 * MIN)) % 2 == 0 { 0.5 } else { -0.5 };
            let down = if after { k.fall } else { 0.0 };
            let ab = 25.0 + jig - down;
            let noise = -95.0 + if after { k.noise_rise } else { 0.0 };
            let mut links = vec![("b", ab, noise)];
            if let Some(c) = k.c_fall {
                links.push(("c", 22.0 + jig - if after { c } else { 0.0 }, noise));
            }
            poll_n(&mut l, t, "a", "4.26.7.0", &links);
            let ba = 24.0 + jig - if k.both_ends { down } else { 0.0 };
            poll_n(&mut l, t + MIN, "b", "4.26.7.0", &[("a", ba, -96.0)]);
            t += 30 * MIN;
        }
        analyze_now(&l)
    }

    fn stepped(fall: f64, noise_rise: f64, c_fall: f64) -> Vec<PairView> {
        stepped_with(Steps {
            fall,
            noise_rise,
            c_fall: Some(c_fall),
            ..STEP
        })
    }

    fn step_note(p: &[PairView]) -> String {
        let p = p.iter().find(|p| p.a == "a" && p.b == "b").unwrap();
        p.step_note.clone().unwrap_or_default()
    }

    fn kinds(p: &[PairView], b: &str) -> Vec<AlertKind> {
        let p = p.iter().find(|p| p.a == "a" && p.b == b).unwrap();
        p.alerts.iter().map(|a| a.kind).collect()
    }

    #[test]
    fn a_lasting_step_with_steady_noise_points_at_aim() {
        let p = stepped(8.0, 0.0, 0.0);
        assert_eq!(
            kinds(&p, "b"),
            vec![AlertKind::Aim],
            "and not drift as well"
        );
        let ab = p.iter().find(|p| p.b == "b").unwrap();
        let text = &ab.alerts[0].text;
        assert!(text.contains("stepped down 8 dB"), "{text}");
        let st = ab.sides[0].step.as_ref().unwrap();
        assert!((st.at - (NOW - 3 * DAY)).abs() <= HOUR, "{}", st.at);
        assert!(kinds(&p, "c").is_empty());
    }

    #[test]
    fn a_step_with_rising_noise_or_at_a_whole_site_is_not_aim() {
        assert!(!kinds(&stepped(8.0, 8.0, 0.0), "b").contains(&AlertKind::Aim));
        assert!(!kinds(&stepped(8.0, 0.0, 8.0), "b").contains(&AlertKind::Aim));
        assert!(
            kinds(&stepped(3.0, 0.0, 0.0), "b").is_empty(),
            "too small to matter"
        );
    }

    #[test]
    fn a_step_heard_at_one_end_or_with_nothing_to_compare_is_not_aim() {
        let one = stepped_with(Steps {
            both_ends: false,
            ..STEP
        });
        assert!(!kinds(&one, "b").contains(&AlertKind::Aim));
        assert!(
            step_note(&one).contains("b still hears a as before"),
            "{}",
            step_note(&one)
        );
        let leaf = stepped_with(Steps {
            c_fall: None,
            ..STEP
        });
        assert!(!kinds(&leaf, "b").contains(&AlertKind::Aim));
        assert!(
            step_note(&leaf).contains("no other link"),
            "{}",
            step_note(&leaf)
        );
        let site = stepped(8.0, 0.0, 8.0);
        assert!(
            step_note(&site).contains("link to c fell"),
            "{}",
            step_note(&site)
        );
        let noisy = stepped(8.0, 8.0, 0.0);
        assert!(
            step_note(&noisy).contains("interference"),
            "{}",
            step_note(&noisy)
        );
    }

    #[test]
    fn a_step_that_recovered_is_not_a_step() {
        let p = stepped_with(Steps {
            recovers: true,
            ..STEP
        });
        let ab = p.iter().find(|p| p.b == "b").unwrap();
        assert!(ab.sides.iter().all(|s| s.step.is_none()));
        assert!(!kinds(&p, "b").contains(&AlertKind::Aim));
    }

    #[test]
    fn a_node_losing_every_link_for_good_is_a_loss_not_a_blank_poll() {
        let mut l = log();
        let all = [("b", 20.0), ("c", 20.0), ("d", 20.0)];
        for i in 0..6 {
            poll(&mut l, NOW - (12 - i) * 5 * MIN, "a", &all);
        }
        for i in 6..12 {
            poll(&mut l, NOW - (12 - i) * 5 * MIN, "a", &[]);
        }
        let p = analyze_now(&l);
        assert!(p.iter().all(|p| p.state == LinkState::Down));
        let (n, skipped) = flaps(&p, "c");
        assert_eq!((n, skipped.len()), (1, 0));
    }

    #[test]
    fn a_flap_both_ends_see_counts_and_a_tracker_hiccup_does_not() {
        // a and b polled by different cores, b 2 to 3 minutes after a
        let run = |b_sees_it_drop: bool| {
            let mut l = log();
            for i in 0..24 {
                let t = NOW - (24 - i) * 5 * MIN;
                let up = i % 2 == 0;
                let links: &[(&str, f64)] = if up {
                    &[("b", 20.0), ("c", 20.0)]
                } else {
                    &[("c", 20.0)]
                };
                poll(&mut l, t, "a", links);
                let b_links: &[(&str, f64)] = if up || !b_sees_it_drop {
                    &[("a", 20.0)]
                } else {
                    &[]
                };
                poll(&mut l, t + 150_000, "b", b_links);
            }
            flaps(&analyze_now(&l), "b")
        };
        let (n, skipped) = run(true);
        assert!(n >= 10 && skipped.is_empty(), "{n} {skipped:?}");
        let (n, skipped) = run(false);
        assert_eq!(n, 0);
        assert!(skipped.iter().all(|k| *k == Skipped::OtherEndHeard));
    }
}
