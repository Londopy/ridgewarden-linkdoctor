//! The links page (single points of failure, the next-workday list, every link), a page
//! per link with its history, "what if" for any site or link, and alert acknowledgements.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ridgewarden_client::alerts;
use ridgewarden_client::graph::{Edge, edge};
use ridgewarden_client::svg::{Chart, Line};
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, median, norm_node, now_ms, short_time};
use serde_json::{Value, json};

use crate::App;
use crate::expect::{self, Expect};
use crate::history::{self, DAY, HOUR, Hour};
use crate::links::{self, LinkState, NodeAlert, PairView, Topology, Visit, WEAK_SNR, WorkItem};
use crate::weather::{self, Check};
use crate::{record, upgrade};

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(links_page))
        .route("/link", get(link_page))
        .route("/whatif", get(whatif_page))
        .route("/upgrade", get(upgrade_page))
        .route("/record", get(record_page))
        .route("/ack/{token}", get(ack_page).post(ack_post))
        .route("/api/links", get(links_json))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

pub struct View {
    pub now: i64,
    pub pairs: Vec<PairView>,
    pub nodes: Vec<NodeAlert>,
    pub sites: BTreeSet<String>,
    /// sites whose own links we have fresh data for (polled on a LAN or crawled)
    pub observed: BTreeSet<String>,
    pub topo: Topology,
    pub here: String,
    pub workday: Vec<Visit>,
    /// the weather check, for links with a drift or aim alert
    pub checks: BTreeMap<Edge, Check>,
    /// the planner's predictions
    pub expect: BTreeMap<Edge, Expect>,
}

/// Analyze the whole mesh as seen from `here`. Pages use `App::view`, which caches it.
pub fn analyze(app: &App, here: &str) -> anyhow::Result<View> {
    let now = now_ms();
    let c = app.db();
    let mut pairs = links::analyze(&c, now, app.fresh_ms(), &app.cfg.links)?;
    let expect = expect::all(&c)?;
    for p in &mut pairs {
        if let Some(a) = expect
            .get(&p.edge())
            .and_then(|e| expect::shortfall(p, e, &app.cfg.links))
        {
            p.alerts.push(a);
        }
    }
    let nodes = links::node_alerts(&c, now, &app.cfg.links)?;
    let sites = links::known_sites(&c, now)?;
    let observed = c
        .prepare("SELECT DISTINCT node FROM polls WHERE ok = 1 AND ts >= ?1")?
        .query_map([now - app.fresh_ms()], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    let topo = links::topology(&pairs, &sites, Some(here));
    let mut checks = BTreeMap::new();
    for p in pairs
        .iter()
        .filter(|p| p.alerts.iter().any(|a| weather::applies(a.kind)))
    {
        if let Some(ch) = app.wx.check(&c, &app.cfg.weather, (&p.a, &p.b), now)? {
            checks.insert(p.edge(), ch);
        }
    }
    Ok(View {
        workday: links::workday(&pairs, &nodes, &topo),
        now,
        pairs,
        nodes,
        sites,
        observed,
        topo,
        here: here.to_string(),
        checks,
        expect,
    })
}

fn link_color(s: LinkState) -> Color {
    match s {
        LinkState::Up => Color::Green,
        LinkState::Down => Color::Red,
        LinkState::Unknown => Color::Unknown,
    }
}

fn state_word(s: LinkState) -> &'static str {
    match s {
        LinkState::Up => "UP",
        LinkState::Down => "DOWN",
        LinkState::Unknown => "UNKNOWN",
    }
}

fn opt(v: Option<f64>, unit: &str) -> String {
    v.map(|x| format!("{x:.0}{unit}"))
        .unwrap_or_else(|| "&ndash;".into())
}

/// Percent-encode a query value.
fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            let _ = write!(o, "%{b:02X}");
        }
    }
    o
}

fn names(app: &App, sites: &BTreeSet<String>) -> String {
    sites
        .iter()
        .map(|n| app.cfg.label(n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A workday line in words, with site labels.
pub fn work_text(app: &App, w: &WorkItem) -> String {
    let mut parts = w.reasons.clone();
    if !w.only_path_to.is_empty() {
        parts.push(format!(
            "Weak, and the only path to {}.",
            names(app, &w.only_path_to)
        ));
    } else if !w.affected.is_empty() {
        parts.push(format!("Affects {}.", names(app, &w.affected)));
    }
    parts.join(" ")
}

fn page(here: &str, active: &str, title: &str, body: &str) -> Html<String> {
    let page = Page {
        app: "ridgewarden-linkdoctor",
        nav: &[
            ("/", "Links"),
            ("/whatif", "What if"),
            ("/upgrade", "Upgrade plan"),
            ("/record", "Track record"),
        ],
        active,
        who: here,
        refresh: active == "/",
    };
    Html(page.render(title, body))
}

fn link_href(a: &str, b: &str) -> String {
    format!("/link?a={}&amp;b={}", enc(a), enc(b))
}

fn open_alerts(app: &App) -> Result<String, Fail> {
    let open = alerts::open_alerts(&app.db())?;
    let mut b = String::new();
    if !open.is_empty() {
        b.push_str("<h2>Open alerts</h2><ul class=plain>");
        for a in open {
            let _ = write!(
                b,
                "<li>{} {} <span class=\"mute small\">since {}{}</span></li>",
                pill(
                    if a.severity == "critical" {
                        Color::Red
                    } else {
                        Color::Yellow
                    },
                    &a.severity
                ),
                esc(&a.title),
                esc(&short_time(a.opened_ms)),
                if a.acked { ", acknowledged" } else { "" }
            );
        }
        b.push_str("</ul>");
    }
    for x in app.cfg.alerts.lapsed(now_ms()) {
        let _ = write!(
            b,
            "<div class=banner>The silence on \"{}\" passed its review date ({}). Alerts for it are back on. \
             Reason given: {}</div>",
            esc(&x.matches),
            esc(&x.review),
            esc(&x.reason)
        );
    }
    Ok(b)
}

fn workday_html(app: &App, v: &View) -> String {
    let l = |n: &str| esc(&app.cfg.label(n));
    if v.workday.is_empty() {
        return "<p class=mute>Nothing needs a site visit.</p>".into();
    }
    let mut b = String::from(
        "<p class=\"mute small\">Each item is scored as how bad it is (0 to 3) times one plus the number of sites it affects; sites are ordered by their total.</p>",
    );
    for visit in &v.workday {
        let _ = write!(
            b,
            "<h3>{} <span class=\"mute small\">score {:.1}</span></h3>",
            l(&visit.site),
            visit.score
        );
        if let Some(o) = app.wx.outlook(&visit.site, v.now) {
            let _ = write!(b, "<p class=small>Forecast: {}</p>", esc(&o));
        }
        b.push_str("<ul>");
        for w in &visit.items {
            let what = match &w.link {
                Some((a, c)) => format!(
                    "<a href=\"{}\">{} &harr; {}</a>",
                    link_href(a, c),
                    l(a),
                    l(c)
                ),
                None => format!("{} itself", l(&visit.site)),
            };
            let held = w
                .link
                .as_ref()
                .and_then(|e| v.checks.get(e)?.held.as_deref())
                .map(|h| format!(" <span class=\"mute small\">{}</span>", esc(h)))
                .unwrap_or_default();
            let _ = write!(
                b,
                "<li><b>{what}</b> <span class=\"mute small\">({:.1})</span>: {}{held}</li>",
                w.score,
                esc(&work_text(app, w))
            );
        }
        b.push_str("</ul>");
    }
    b
}

/// The worst problem showing on a site (its links or itself) or on one link, 0 to 3,
/// with what it is. A weak link counts as 1.5, as on the workday list.
fn trouble(v: &View, site: Option<&str>, link: Option<&Edge>) -> (f64, Vec<&'static str>) {
    let mut sev: f64 = 0.0;
    let mut what = BTreeSet::new();
    let alerts = v
        .pairs
        .iter()
        .filter(|p| match (site, link) {
            (Some(s), _) => p.a == s || p.b == s,
            (_, Some(e)) => &p.edge() == e,
            _ => false,
        })
        .flat_map(|p| &p.alerts)
        .chain(
            v.nodes
                .iter()
                .filter(|n| site == Some(n.node.as_str()))
                .map(|n| &n.alert),
        );
    for a in alerts {
        sev = sev.max(a.severity);
        what.insert(a.kind.word());
    }
    if let Some(e) = link
        && v.pairs.iter().any(|p| &p.edge() == e && p.weak())
    {
        sev = sev.max(1.5);
        what.insert("weak");
    }
    (sev, what.into_iter().collect())
}

async fn links_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = app.view(&here)?;
    let l = |n: &str| esc(&app.cfg.label(n));
    let mut b = String::from("<h1>Links</h1>");
    b.push_str(&open_alerts(&app)?);

    b.push_str("<h2>Single points of failure</h2>");
    // the graph only holds links that some core can see; say so when that's not everything
    let unseen: Vec<String> = v
        .sites
        .iter()
        .filter(|n| !v.observed.contains(*n))
        .map(|n| app.cfg.label(n))
        .collect();
    if !unseen.is_empty() {
        let _ = write!(
            b,
            "<p class=\"banner small\">Partial picture: {} of {} sites report their links ({} do not). \
             Links between sites without a ridgewarden core are invisible, so this list can be wrong. \
             Turn on <code>[crawl]</code> on one core to fill it in.</p>",
            v.sites.len() - unseen.len(),
            v.sites.len(),
            esc(&unseen.join(", "))
        );
    }
    let from = if here.is_empty() {
        "here".to_string()
    } else {
        l(&here)
    };
    if v.topo.aps.is_empty() && v.topo.bridges.is_empty() {
        b.push_str("<p>No single site or link failure would cut anyone off.</p>");
    } else {
        // ranked by risk: the sites cut off, weighted by how unwell the thing already is
        let line =
            |what: String, cut: &BTreeSet<String>, from: &str, (sev, showing): (f64, Vec<&str>)| {
                let text = format!(
                    "<li>If {what} fails, {} {} cut off from {from}.{}</li>",
                    esc(&names(&app, cut)),
                    if cut.len() == 1 { "is" } else { "are" },
                    if showing.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " <span class=small>Showing now: {}.</span>",
                            esc(&showing.join(", "))
                        )
                    }
                );
                (cut.len() as f64 * (1.0 + sev), text)
            };
        let mut risks: Vec<(f64, String)> = v
            .topo
            .aps
            .iter()
            .map(|(x, cut)| {
                // losing this site itself leaves nothing to be cut off "from here"
                let side = if *x == here {
                    "the rest of the mesh"
                } else {
                    &from
                };
                line(
                    format!("site <b>{}</b>", l(x)),
                    cut,
                    side,
                    trouble(&v, Some(x), None),
                )
            })
            .chain(v.topo.bridges.iter().map(|(e, cut)| {
                let what = format!(
                    "the <a href=\"{}\"><b>{} &harr; {}</b></a> link",
                    link_href(&e.0, &e.1),
                    l(&e.0),
                    l(&e.1)
                );
                line(what, cut, &from, trouble(&v, None, Some(e)))
            }))
            .collect();
        risks.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
        b.push_str("<ul class=plain>");
        for (_, li) in risks {
            b.push_str(&li);
        }
        b.push_str("</ul>");
    }

    if !v.nodes.is_empty() {
        b.push_str("<h2>Sites</h2><ul class=plain>");
        for n in &v.nodes {
            let _ = write!(b, "<li><b>{}</b>: {}</li>", l(&n.node), esc(&n.alert.text));
        }
        b.push_str("</ul>");
    }

    b.push_str("<h2>Next workday</h2>");
    b.push_str(&workday_html(&app, &v));

    let planned = !v.expect.is_empty();
    let _ = write!(
        b,
        "<h2>All links</h2><div class=wrap><table><tr><th>Link</th><th>Type</th><th>State</th><th>SNR</th>{}<th>Quality</th><th>Summary</th></tr>",
        if planned { "<th>Expected</th>" } else { "" }
    );
    let mut pairs: Vec<_> = v.pairs.iter().collect();
    pairs.sort_by(|a, b| {
        b.severity()
            .total_cmp(&a.severity())
            .then_with(|| a.edge().cmp(&b.edge()))
    });
    for p in pairs {
        let _ = write!(
            b,
            "<tr><td><a href=\"{}\">{} &harr; {}</a></td><td>{}</td><td>{}</td><td>{}</td>{}<td>{}</td><td>{}</td></tr>",
            link_href(&p.a, &p.b),
            l(&p.a),
            l(&p.b),
            esc(&p.link_type),
            pill(link_color(p.state), state_word(p.state)),
            opt(p.snr(), " dB"),
            if planned {
                format!(
                    "<td>{}</td>",
                    opt(v.expect.get(&p.edge()).map(|e| e.snr), " dB")
                )
            } else {
                String::new()
            },
            opt(p.quality(), "%"),
            esc(&p.summary(v.now))
        );
    }
    b.push_str("</table></div>");
    Ok(page(&v.here, "/", "Links", &b))
}

/// Hourly values bucketed to `width`, by median, so a year of history stays a light page.
fn series(hours: &[Hour], width: i64, f: fn(&Hour) -> Option<f64>) -> Vec<(i64, f64)> {
    let mut out: Vec<(i64, f64)> = vec![];
    let mut bucket: Vec<f64> = vec![];
    let mut start = None;
    for h in hours {
        let b = h.hour - h.hour.rem_euclid(width);
        if start.is_some_and(|s| s != b) {
            if let Some(m) = median(&mut bucket) {
                out.push((start.unwrap_or(b), m));
            }
            bucket.clear();
        }
        start = Some(b);
        if let Some(v) = f(h) {
            bucket.push(v);
        }
    }
    if let (Some(s), Some(m)) = (start, median(&mut bucket)) {
        out.push((s, m));
    }
    out
}

async fn link_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let here = app.here().await;
    let (a, c) = edge(
        &norm_node(q.get("a").map_or("", |s| s)),
        &norm_node(q.get("b").map_or("", |s| s)),
    );
    let days: i64 = match q.get("days").map(String::as_str) {
        Some("30") => 30,
        Some("365") => 365,
        _ => 7,
    };
    let v = app.view(&here)?;
    let l = |n: &str| esc(&app.cfg.label(n));
    let Some(p) = v.pairs.iter().find(|p| p.a == a && p.b == c) else {
        let body = "<h1>Not found</h1><p>No link between those sites in the last week. <a href=\"/\">All links</a></p>";
        return Ok((StatusCode::NOT_FOUND, page(&here, "", "Not found", body)).into_response());
    };
    let now = v.now;
    let from = now - days * DAY;
    let width = match days {
        7 => HOUR,
        30 => 4 * HOUR,
        _ => DAY,
    };
    let (marks, wx_marks) = {
        let db = app.db();
        (
            history::marks(&db, &a, &c)?,
            app.wx.marks(&db, &app.cfg.weather, (&a, &c), from, now)?,
        )
    };
    let mut b = String::new();
    let _ = write!(
        b,
        "<h1>{} &harr; {}</h1><p>{} {} <span class=\"mute small\">{}</span></p><p>{}</p>",
        l(&a),
        l(&c),
        pill(link_color(p.state), state_word(p.state)),
        esc(&p.link_type),
        esc(&p.last_seen.map(short_time).unwrap_or_default()),
        esc(&p.summary(now))
    );
    if p.alerts.iter().any(|x| x.kind == links::AlertKind::Loss) {
        let _ = write!(
            b,
            "<p>{}</p>",
            esc(&p
                .alerts
                .iter()
                .map(|x| x.text.clone())
                .collect::<Vec<_>>()
                .join(" "))
        );
    }
    if let Some(ch) = v.checks.get(&p.edge()) {
        if let Some(h) = &ch.held {
            let _ = write!(b, "<p class=banner>{}</p>", esc(h));
        }
        if !ch.note.is_empty() {
            let _ = write!(b, "<p class=\"mute small\">{}</p>", esc(&ch.note));
        }
    }
    let expected = v.expect.get(&p.edge());
    if let Some(e) = expected {
        let _ = write!(
            b,
            "<p>Should be {:.0} dB SNR by {} <span class=\"mute small\">(from {})</span>; measures {}.</p>",
            e.snr,
            esc(&e.source()),
            esc(&short_time(e.ts)),
            opt(expect::measured(p), " dB")
        );
    }
    let held = app.cfg.alerts.hold(
        &format!("link {a}|{c} {} - {}", app.cfg.label(&a), app.cfg.label(&c)),
        now,
    );
    if let Some(h) = held {
        let _ = write!(b, "<p class=banner>{}</p>", esc(&h));
    }
    b.push_str("<p class=noprint>");
    for (d, label) in [(7, "7 days"), (30, "30 days"), (365, "1 year")] {
        if d == days {
            let _ = write!(b, "<b>{label}</b> &middot; ");
        } else {
            let _ = write!(
                b,
                "<a href=\"{}&amp;days={d}\">{label}</a> &middot; ",
                link_href(&a, &c)
            );
        }
    }
    b.push_str("</p>");

    let ends = [(a.clone(), c.clone()), (c.clone(), a.clone())];
    let hist: Vec<(String, String, Vec<Hour>)> = {
        let db = app.db();
        ends.iter()
            .map(|(x, y)| Ok((x.clone(), y.clone(), history::hours(&db, x, y, from, now)?)))
            .collect::<anyhow::Result<_>>()?
    };
    let lines = |f: fn(&Hour) -> Option<f64>, what: &str| -> Vec<Line> {
        hist.iter()
            .filter(|(_, _, h)| !h.is_empty())
            .map(|(x, y, h)| Line {
                label: format!("{} {what} {}", app.cfg.label(x), app.cfg.label(y)),
                points: series(h, width, f),
            })
            .collect()
    };
    let rf = p.link_type == "RF";
    let chart = |title: &str, unit: &str, lines: Vec<Line>, rule: Option<(f64, String)>| {
        Chart {
            from,
            to: now,
            unit: unit.into(),
            lines,
            markers: marks
                .iter()
                .map(|m| (m.ts, m.text.clone()))
                .chain(wx_marks.iter().cloned())
                .collect(),
            rule,
            gap_ms: 3 * width,
        }
        .render(title)
    };
    if rf {
        b.push_str("<h2>Signal to noise</h2><p class=\"mute small\">Median for each end. Yellow lines are firmware changes, notes and rain; hover for details.</p>");
        let mut snr = lines(|h| h.snr, "hears");
        let step = p
            .sides
            .iter()
            .filter_map(|s| Some((s, s.step.as_ref()?)))
            .find(|(_, st)| st.at >= from);
        if let Some((_, st)) = step {
            // a point every bucket, or the chart's gap rule would leave the levels unjoined
            let level = |a: i64, z: i64, y: f64| {
                (0..)
                    .map(move |k| a + k * width)
                    .take_while(move |t| *t < z)
                    .chain([z])
                    .map(move |t| (t, y))
            };
            snr.push(Line {
                label: "level before and after the step".into(),
                points: level(st.from.max(from), st.at, st.before)
                    .chain(level(st.at, now, st.after))
                    .collect(),
            });
        }
        let rule = match expected {
            Some(e) => (e.snr, format!("{:.0} dB: {}", e.snr, e.source())),
            None => (WEAK_SNR, format!("{WEAK_SNR:.0} dB: weak below this")),
        };
        b.push_str(&chart("SNR from each end", "dB", snr, Some(rule)));
        if let (Some((s, st)), Some(why)) = (step, &p.step_note) {
            let _ = write!(
                b,
                "<p>{} hears {} {:.0} dB worse since {}. That isn't called an aim problem because {}.</p>",
                l(&s.node),
                l(&s.neighbor),
                st.fall(),
                esc(&short_time(st.at)),
                esc(why)
            );
        }
    }
    b.push_str("<h2>Link quality</h2>");
    b.push_str(&chart(
        "Link quality from each end",
        "%",
        lines(|h| h.quality, "receives"),
        None,
    ));
    if rf {
        b.push_str("<h2>Noise floor</h2>");
        b.push_str(&chart(
            "Noise floor at each end",
            "dBm",
            lines(|h| h.noise, "noise, link to"),
            None,
        ));
    }

    b.push_str("<h2>Each end</h2><div class=wrap><table><tr><th>End</th><th>Hears at</th><th>Quality (theirs)</th><th>Drops in 24 h</th><th>Trend</th></tr>");
    for s in &p.sides {
        let latest = s.latest.as_ref();
        let trend = match &s.drift {
            Some(d) => format!(
                "{} {:.1} a week since {} ({:.0}% of the data agrees)",
                if d.per_week >= 0.0 {
                    "losing"
                } else {
                    "gaining"
                },
                d.per_week.abs(),
                short_time(d.since),
                d.agreement * 100.0
            ),
            None => "not enough history yet".into(),
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{} ({})</td><td>{}</td><td>{}</td></tr>",
            l(&s.node),
            opt(latest.and_then(|x| x.snr), " dB"),
            opt(latest.and_then(|x| x.quality), "%"),
            opt(latest.and_then(|x| x.nquality), "%"),
            if s.skipped_24h.is_empty() {
                s.drops_24h.to_string()
            } else {
                format!("{} ({} left out)", s.drops_24h, s.skipped_24h.len())
            },
            esc(&trend)
        );
    }
    b.push_str("</table></div>");
    let skipped: BTreeMap<links::Skipped, usize> = p
        .sides
        .iter()
        .flat_map(|s| &s.skipped_24h)
        .fold(BTreeMap::new(), |mut m, k| {
            *m.entry(*k).or_default() += 1;
            m
        });
    if !skipped.is_empty() {
        let why: Vec<String> = skipped
            .iter()
            .map(|(k, n)| format!("{n} because {}", k.text()))
            .collect();
        let _ = write!(
            b,
            "<p class=\"mute small\">Polls that missed this link but weren't counted as drops: {}. Polls that failed or timed out never count.</p>",
            esc(&why.join("; "))
        );
    }
    if !marks.is_empty() {
        b.push_str("<h2>Changes and notes</h2><ul class=plain>");
        for m in marks.iter().rev() {
            let _ = write!(
                b,
                "<li>{} <span class=\"mute small\">{}{}</span></li>",
                esc(&m.text),
                esc(&short_time(m.ts)),
                if m.change { ", radio change" } else { "" }
            );
        }
        b.push_str("</ul>");
    }
    b.push_str("<p class=\"mute small\">Add a note with <code>ridgewarden-linkdoctor note &lt;site&gt; --to &lt;other site&gt; \"what you did\"</code>; add <code>--change</code> when it changed the radio path, so the trend restarts from there.</p>");
    Ok(page(&here, "", &format!("{a} - {c}"), &b).into_response())
}

async fn whatif_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = app.view(&here)?;
    let l = |n: &str| esc(&app.cfg.label(n));
    let mut b = String::from(
        "<h1>What if</h1><p>Pick a site or a link to see who would be cut off if it failed, \
         given the links that are up now.</p><form method=get action=\"/whatif\">\
         <label>Site <select name=site><option value=\"\">none</option>",
    );
    let site = q
        .get("site")
        .map(|s| norm_node(s))
        .filter(|s| v.sites.contains(s));
    let link: Option<Edge> = q
        .get("link")
        .and_then(|s| s.split_once('|'))
        .map(|(x, y)| edge(&norm_node(x), &norm_node(y)))
        .filter(|e| v.topo.graph.edges().contains(e));
    for s in &v.sites {
        let sel = if site.as_ref() == Some(s) {
            " selected"
        } else {
            ""
        };
        let _ = write!(b, "<option value=\"{}\"{sel}>{}</option>", esc(s), l(s));
    }
    b.push_str("</select></label> <label>Link <select name=link><option value=\"\">none</option>");
    for (x, y) in v.topo.graph.edges() {
        let val = format!("{x}|{y}");
        let sel = if link.as_ref() == Some(&(x.clone(), y.clone())) {
            " selected"
        } else {
            ""
        };
        let _ = write!(
            b,
            "<option value=\"{}\"{sel}>{} - {}</option>",
            esc(&val),
            l(&x),
            l(&y)
        );
    }
    b.push_str("</select></label> <button type=submit>Show</button></form>");
    if site.is_some() || link.is_some() {
        let nodes: BTreeSet<String> = site.iter().cloned().collect();
        let edges: BTreeSet<Edge> = link.iter().cloned().collect();
        let from = Some(here.as_str()).filter(|h| !h.is_empty() && site.as_deref() != Some(*h));
        let cut: BTreeSet<String> = v
            .topo
            .graph
            .what_if(&nodes, &edges, from)
            .difference(&v.topo.cut_now)
            .filter(|n| site.as_ref() != Some(*n))
            .cloned()
            .collect();
        let _ = write!(
            b,
            "<h2>Result</h2><p>{}</p>",
            if cut.is_empty() {
                "Nobody else would be cut off.".to_string()
            } else {
                format!(
                    "{} would be cut off{}.",
                    esc(&names(&app, &cut)),
                    match from {
                        Some(h) => format!(" from {}", l(h)),
                        None => " from the rest of the mesh".into(),
                    }
                )
            }
        );
    }
    Ok(page(&v.here, "/whatif", "What if", &b))
}

async fn upgrade_page(
    State(app): State<Arc<App>>,
    Query(q): Query<Vec<(String, String)>>,
) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = app.view(&here)?;
    let l = |n: &str| esc(&app.cfg.label(n));
    let fw = upgrade::firmware(&app.db(), v.now - 7 * DAY)?;
    let arg = |k: &'static str| {
        q.iter()
            .filter(move |(x, _)| x == k)
            .map(|(_, y)| y.as_str())
    };
    let target: String = arg("to")
        .next()
        .map(|t| t.trim().chars().take(40).collect::<String>())
        .filter(|t| !t.is_empty())
        .or_else(|| upgrade::newest(fw.values()).cloned())
        .unwrap_or_default();
    let planned = arg("plan").next().is_some();
    let picked: BTreeSet<String> = if planned {
        arg("site")
            .map(norm_node)
            .filter(|s| v.sites.contains(s))
            .collect()
    } else {
        fw.iter()
            .filter(|(_, f)| upgrade::behind(f, &target))
            .map(|(n, _)| n.clone())
            .collect()
    };
    let mut b = String::from(
        "<h1>Upgrade plan</h1><p>Upgrade the far end first. A node restarts during a firmware \
         upgrade and may not come back, so each site goes only when no site still waiting \
         for its upgrade is reached through it from here. Sites behind the target release are \
         ticked; tick or untick any, then plan.</p>",
    );
    if here.is_empty() {
        b.push_str("<p class=banner>This site's own node isn't known yet (the core hasn't polled it), so there is no \"here\" to plan from.</p>");
        return Ok(page(&v.here, "/upgrade", "Upgrade plan", &b));
    }
    let _ = write!(
        b,
        "<form method=get action=\"/upgrade\"><input type=hidden name=plan value=1>\
         <p><label>Target firmware <input name=to value=\"{}\"></label></p>\
         <div class=wrap><table><tr><th></th><th>Site</th><th>Firmware now</th></tr>",
        esc(&target)
    );
    for s in &v.sites {
        let _ = write!(
            b,
            "<tr><td><input type=checkbox name=site value=\"{}\" id=\"u-{}\"{}></td>\
             <td><label for=\"u-{}\">{}</label></td><td>{}</td></tr>",
            esc(s),
            esc(s),
            if picked.contains(s) { " checked" } else { "" },
            esc(s),
            l(s),
            fw.get(s)
                .map_or("<span class=mute>not polled</span>".into(), |f| esc(f))
        );
    }
    b.push_str("</table></div><p><button type=submit>Plan</button></p></form>");
    if picked.is_empty() {
        b.push_str("<p class=mute>Nothing is ticked.</p>");
        return Ok(page(&v.here, "/upgrade", "Upgrade plan", &b));
    }
    let plan = upgrade::plan(&v.topo.graph, &here, &picked);
    let _ = write!(b, "<h2>Order, seen from {}</h2><ol>", l(&here));
    let mut done = BTreeSet::new();
    for st in &plan.steps {
        let (upgraded, others): (BTreeSet<String>, BTreeSet<String>) =
            st.dark.iter().cloned().partition(|n| done.contains(n));
        let mut dark = vec![];
        if !upgraded.is_empty() {
            dark.push(format!("{} (done already)", names(&app, &upgraded)));
        }
        if !others.is_empty() {
            dark.push(format!("{} (not in this plan)", names(&app, &others)));
        }
        let _ = write!(
            b,
            "<li><b>{}</b> <span class=\"mute small\">{}, on {}</span>{}</li>",
            l(&st.site),
            if st.site == here {
                "this site, last".to_string()
            } else {
                format!("{} hop{}", st.hops, if st.hops == 1 { "" } else { "s" })
            },
            esc(fw.get(&st.site).map_or("unknown firmware", String::as_str)),
            if dark.is_empty() {
                String::new()
            } else {
                format!(
                    "<br><span class=small>While it restarts, out of reach from here: {}.</span>",
                    esc(&dark.join("; "))
                )
            }
        );
        done.insert(st.site.clone());
    }
    b.push_str("</ol>");
    if !plan.unreachable.is_empty() {
        let _ = write!(
            b,
            "<p class=banner>Not reachable from here now: {}. Upgrade those from a site that can reach them, \
             or once the path is back.</p>",
            esc(&names(&app, &plan.unreachable))
        );
    }
    b.push_str("<p class=\"mute small\">Planned on the links up now; links with no fresh data are taken as up. \
         Hops are counted on this graph, not on OLSR or Babel metrics.</p>");
    Ok(page(&v.here, "/upgrade", "Upgrade plan", &b))
}

async fn record_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let l = |n: &str| esc(&app.cfg.label(n));
    let now = now_ms();
    let (losses, visits) = {
        let db = app.db();
        (record::losses(&db)?, record::visits(&db, now)?)
    };
    let mut b = String::from(
        "<h1>Track record</h1><p>What Link Doctor found and whether it helped, from this site's \
         own record of what each analysis showed. Kept 400 days.</p><h2>Warnings before failures</h2>",
    );
    if losses.is_empty() {
        b.push_str("<p class=mute>No link has been lost since the record started.</p>");
    } else {
        let caught = losses.iter().filter(|x| x.warning.is_some()).count();
        let _ = write!(
            b,
            "<p><b>{caught} of {}</b> lost links had a warning first (showing in the month before).</p>\
             <div class=wrap><table><tr><th>Link</th><th>Lost</th><th>Warning first</th></tr>",
            losses.len()
        );
        for x in &losses {
            let _ = write!(
                b,
                "<tr><td><a href=\"{}\">{} &harr; {}</a></td><td>{}</td><td>{}</td></tr>",
                link_href(&x.a, &x.b),
                l(&x.a),
                l(&x.b),
                esc(&short_time(x.at)),
                match x.warning {
                    Some((k, t)) => esc(&format!(
                        "{} from {}, {} ahead",
                        k.word(),
                        short_time(t),
                        human_duration(x.at - t)
                    )),
                    None => "<span class=mute>none</span>".into(),
                }
            );
        }
        b.push_str("</table></div>");
    }
    b.push_str("<h2>Site visits</h2>");
    let with_problems: Vec<&record::Visit> = visits.iter().filter(|x| x.open > 0).collect();
    if visits.is_empty() {
        b.push_str("<p class=mute>No visits noted yet. After a trip, add a note with <code>ridgewarden-linkdoctor note</code>.</p>");
    } else if with_problems.is_empty() {
        let _ = write!(
            b,
            "<p>None of the {} notes so far were made while a problem was showing at that site.</p>",
            visits.len()
        );
    } else {
        // only visits a week old or more can show whether a fix held
        let judged = with_problems.iter().filter(|x| !x.pending);
        let open: usize = judged.clone().map(|x| x.open).sum();
        let cleared: usize = judged.map(|x| x.cleared).sum();
        let _ = write!(
            b,
            "<p><b>{}</b> of {} notes were made at sites with problems showing. Of the {open} problems \
             at visits a week old or more, {cleared} cleared within a day and stayed cleared for a week.</p>",
            with_problems.len(),
            visits.len()
        );
    }
    if !visits.is_empty() {
        b.push_str("<ul class=plain>");
        for x in &visits {
            let _ = write!(
                b,
                "<li>{} <b>{}</b>: {} <span class=\"mute small\">{}, {}</span></li>",
                esc(&short_time(x.ts)),
                l(&x.node),
                esc(&x.text),
                esc(&x.call),
                match (x.open, x.pending) {
                    (0, _) => "nothing was showing there".to_string(),
                    (n, true) => format!("{n} showing, too soon to say if it stayed fixed"),
                    (n, false) => format!("{n} showing, {} cleared", x.cleared),
                }
            );
        }
        b.push_str("</ul>");
    }
    Ok(page(&here, "/record", "Track record", &b))
}

async fn ack_page(State(app): State<Arc<App>>, Path(token): Path<String>) -> Html<String> {
    let here = app.here().await;
    // a GET only shows the button, so link previews in chat apps can't acknowledge anything
    let b = format!(
        "<h1>Acknowledge alert</h1><p>Acknowledging stops repeats of this alert. You'll still hear \
         when it's resolved.</p><form method=post action=\"/ack/{}\"><button type=submit>Acknowledge</button></form>",
        enc(&token)
    );
    page(&here, "", "Acknowledge", &b)
}

async fn ack_post(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
) -> Result<Response, Fail> {
    let here = app.here().await;
    let done = alerts::ack(&app.db(), &token, now_ms())?;
    let (code, b) = match done {
        Some(title) => (
            StatusCode::OK,
            format!(
                "<h1>Acknowledged</h1><p>{}</p><p><a href=\"/\">All links</a></p>",
                esc(&title)
            ),
        ),
        None => (
            StatusCode::NOT_FOUND,
            "<h1>Nothing to acknowledge</h1><p>That alert is already resolved, or the link is wrong.</p>"
                .to_string(),
        ),
    };
    Ok((code, page(&here, "", "Acknowledge", &b)).into_response())
}

async fn links_json(State(app): State<Arc<App>>) -> Result<Json<Value>, Fail> {
    let here = app.here().await;
    let v = app.view(&here)?;
    let pairs: Vec<Value> = v
        .pairs
        .iter()
        .map(|p| {
            json!({
                "a": p.a, "b": p.b, "type": p.link_type,
                "state": state_word(p.state),
                "snr": p.snr(), "quality": p.quality(), "last_seen": p.last_seen,
                "summary": p.summary(v.now),
                "drift": p.sides.iter().map(|s| json!({
                    "node": s.node,
                    "per_week": s.drift.as_ref().map(|d| d.per_week),
                    "since": s.drift.as_ref().map(|d| d.since),
                    "agreement": s.drift.as_ref().map(|d| d.agreement),
                })).collect::<Vec<_>>(),
                "ends": p.sides.iter().map(|s| json!({
                    "node": s.node,
                    "drops_24h": s.drops_24h,
                    "skipped_24h": s.skipped_24h.iter().map(|k| format!("{k:?}")).collect::<Vec<_>>(),
                    "step": s.step.as_ref().map(|st| json!({
                        "at": st.at, "before": st.before, "after": st.after, "noise_change": st.noise_change,
                    })),
                })).collect::<Vec<_>>(),
                "expected_snr": v.expect.get(&p.edge()).map(|e| e.snr),
                "weather": v.checks.get(&p.edge()).map(|c| json!({"held": c.held, "note": c.note})),
                "alerts": p.alerts.iter().map(|a| json!({"kind": format!("{:?}", a.kind), "severity": a.severity, "text": a.text})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let aps: Vec<Value> = v
        .topo
        .aps
        .iter()
        .map(|(x, cut)| json!({"site": x, "cuts_off": cut}))
        .collect();
    let bridges: Vec<Value> = v
        .topo
        .bridges
        .iter()
        .map(|((a, b), cut)| json!({"a": a, "b": b, "cuts_off": cut}))
        .collect();
    let sites: Vec<Value> = v
        .nodes
        .iter()
        .map(|n| json!({"site": n.node, "kind": format!("{:?}", n.alert.kind), "text": n.alert.text}))
        .collect();
    let work: Vec<Value> = v
        .workday
        .iter()
        .map(|w| {
            json!({"site": w.site, "score": w.score, "items": w.items.iter().map(|i| json!({
                "link": i.link, "score": i.score, "risk": i.risk, "affects": i.affected, "reasons": i.reasons,
            })).collect::<Vec<_>>()})
        })
        .collect();
    Ok(Json(json!({
        "generated": v.now, "links": pairs, "sites": sites, "workday": work,
        "articulation_points": aps, "bridges": bridges,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(hour: i64, snr: f64) -> Hour {
        Hour {
            hour,
            n: 1,
            snr: Some(snr),
            snr_p10: None,
            snr_p90: None,
            quality: None,
            nquality: None,
            noise: None,
        }
    }

    #[test]
    fn long_ranges_are_bucketed_by_median() {
        let hours: Vec<Hour> = (0..48)
            .map(|i| h(i * HOUR, if i < 24 { 10.0 } else { 20.0 }))
            .collect();
        assert_eq!(series(&hours, DAY, |h| h.snr), vec![(0, 10.0), (DAY, 20.0)]);
        assert_eq!(series(&hours, HOUR, |h| h.snr).len(), 48);
    }
}
