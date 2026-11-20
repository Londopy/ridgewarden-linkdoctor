//! `ridgewarden-linkdoctor.toml`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ridgewarden_client::alerts::AlertCfg;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// the ridgewarden core on this box
    #[serde(default = "d_core_url")]
    pub core_url: String,
    /// the core's `local.token`; only needed for `ridgewarden-linkdoctor note`
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// link data older than this counts as stale ("unknown"); make it a few poll rounds
    /// (or crawl rounds, if the core crawls)
    #[serde(default = "d_fresh")]
    pub fresh_s: u64,
    #[serde(default)]
    pub links: LinksCfg,
    /// friendly site names: node name -> label, e.g. "n0call-cuesta" = "Cuesta Peak"
    #[serde(default)]
    pub site_names: BTreeMap<String, String>,
    #[serde(default)]
    pub weather: WeatherCfg,
    /// Configure this on one site only, or every site sends the same alert.
    #[serde(default)]
    pub alerts: AlertCfg,
}

/// Where to check the weather before a drift or aim alert goes out.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct WeatherCfg {
    /// turns the NWS check on; api.weather.gov wants the app and a contact in it
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default = "d_nws")]
    pub nws_url: String,
    /// node -> NWS observation station, where the nearest to the node isn't the right one
    #[serde(default)]
    pub stations: BTreeMap<String, String>,
    /// the weather app (spec 09) wherever it runs, e.g. "http://127.0.0.1:8801"
    #[serde(default)]
    pub app_url: Option<String>,
    /// use the weather app's `weather.candidate` events from the core's log
    #[serde(default = "d_true")]
    pub feed: bool,
    /// drift and aim alerts wait until it has been dry at both ends this long
    #[serde(default = "d_dry")]
    pub dry_hours: u32,
}

impl Default for WeatherCfg {
    fn default() -> Self {
        Self {
            user_agent: None,
            nws_url: d_nws(),
            stations: BTreeMap::new(),
            app_url: None,
            feed: true,
            dry_hours: d_dry(),
        }
    }
}

impl WeatherCfg {
    /// Whether there is anything to fetch: NWS or the weather app's own page.
    pub fn on(&self) -> bool {
        self.user_agent.is_some() || self.app_url.is_some()
    }

    /// The station configured for `node`, if any.
    pub fn station(&self, node: &str) -> Option<&str> {
        self.stations
            .iter()
            .find(|(k, _)| ridgewarden_client::util::norm_node(k) == node)
            .map(|(_, v)| v.as_str())
    }

    fn validate(&self) -> Result<()> {
        if self
            .user_agent
            .as_deref()
            .is_some_and(|u| u.trim().is_empty() || u.chars().any(char::is_control))
        {
            bail!(
                "weather.user_agent must name the app and a contact, e.g. \"ridgewarden-linkdoctor (ops@example.org)\""
            );
        }
        for u in std::iter::once(&self.nws_url).chain(&self.app_url) {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                bail!("{u:?} is not an http:// or https:// address");
            }
        }
        for (node, st) in &self.stations {
            if !crate::weather::station_ok(st) {
                bail!("weather.stations: {st:?} for {node} is not an NWS station id like KSBP");
            }
        }
        if !(1..=48).contains(&self.dry_hours) {
            bail!("weather.dry_hours must be from 1 to 48");
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct LinksCfg {
    /// SNR drop over a week that raises a drift alert
    #[serde(default = "d_drift")]
    pub drift_db: f64,
    /// link drops in 24 h that count as flapping
    #[serde(default = "d_flap")]
    pub flap_count: usize,
    /// quality difference between the two directions (percentage points) that counts as one-way
    #[serde(default = "d_one_way_points")]
    pub one_way_points: f64,
    /// SNR difference between the two ends that counts as one-way
    #[serde(default = "d_one_way_db")]
    pub one_way_db: f64,
    /// noise floor rise over the usual for that hour that raises an alert
    #[serde(default = "d_noise_rise")]
    pub noise_rise_db: f64,
    /// SNR step down, held for a day with a steady noise floor, that counts as an aim problem
    #[serde(default = "d_aim_step")]
    pub aim_step_db: f64,
    /// how far below the planner's prediction a link may measure before it goes on the workday list
    #[serde(default = "d_below_plan")]
    pub below_plan_db: f64,
}

fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-linkdoctor")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8792".parse().unwrap()
}
fn d_fresh() -> u64 {
    960
}
fn d_drift() -> f64 {
    3.0
}
fn d_flap() -> usize {
    5
}
fn d_one_way_points() -> f64 {
    30.0
}
fn d_one_way_db() -> f64 {
    10.0
}
fn d_noise_rise() -> f64 {
    6.0
}
fn d_aim_step() -> f64 {
    6.0
}
fn d_below_plan() -> f64 {
    10.0
}
fn d_nws() -> String {
    "https://api.weather.gov".into()
}
fn d_true() -> bool {
    true
}
fn d_dry() -> u32 {
    24
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}

impl Default for LinksCfg {
    fn default() -> Self {
        Self {
            drift_db: d_drift(),
            flap_count: d_flap(),
            one_way_points: d_one_way_points(),
            one_way_db: d_one_way_db(),
            noise_rise_db: d_noise_rise(),
            aim_step_db: d_aim_step(),
            below_plan_db: d_below_plan(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if cfg.fresh_s < 60 {
            bail!("fresh_s must be at least 60");
        }
        let l = &cfg.links;
        if l.drift_db <= 0.0
            || l.one_way_points <= 0.0
            || l.one_way_db <= 0.0
            || l.noise_rise_db <= 0.0
            || l.aim_step_db <= 0.0
            || l.below_plan_db <= 0.0
        {
            bail!("the [links] thresholds must be above zero");
        }
        if l.flap_count == 0 {
            bail!("links.flap_count must be at least 1");
        }
        cfg.weather.validate()?;
        cfg.alerts.validate()?;
        Ok(cfg)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("linkdoctor.db")
    }

    pub fn label(&self, node: &str) -> String {
        self.site_names
            .iter()
            .find(|(k, _)| ridgewarden_client::util::norm_node(k) == node)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| node.to_string())
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-linkdoctor configuration
core_url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"
data_dir = "/var/lib/ridgewarden-linkdoctor"
listen = "0.0.0.0:8792"
fresh_s = 960

[links]
drift_db = 3.0         # SNR lost per week, sustained, before it's flagged
flap_count = 5         # drops in 24 h that count as flapping
one_way_points = 30.0  # quality gap between the two directions that counts as one-way
one_way_db = 10.0      # SNR gap between the two ends that counts as one-way
noise_rise_db = 6.0    # noise floor above the usual for that hour of day
aim_step_db = 6.0      # SNR step down that stays a day with a steady noise floor: antenna aim
below_plan_db = 10.0   # this far below the planner's prediction goes on the workday list

[site_names]
# "n0call-cuesta" = "Cuesta Peak"

# Check the weather before a drift or aim alert goes out: rain near either end holds it
# until it has been dry for dry_hours. Either source is enough; both is better.
[weather]
# NWS observations and forecasts from api.weather.gov, while the internet is up. The
# NWS asks for the app's name and a contact. Each node's nearest station is found from
# the position in its polls; set one by hand where that's wrong.
# user_agent = "ridgewarden-linkdoctor (ops@example.org)"
# the weather app (spec 09) needs no internet: its weather.candidate events reach every
# site through the log. Point app_url at it only if this site's core doesn't get them.
feed = true
# app_url = "http://127.0.0.1:8801"
dry_hours = 24          # 1 to 48
# [weather.stations]
# "n0call-cuesta" = "KSBP"

# Alerts: set these up on one site only, or every site sends the same alert.
[alerts]
# ntfy = "http://10.1.2.3/mesh-alerts"
# discord = "https://discord.com/api/webhooks/..."
# webhooks = []
# public_url = "http://n0call-pi.local.mesh:8792"
# quiet_hours = "22-6"
# repeat_hours = 12

# Mail goes through a local sendmail-compatible program. msmtp works with the shipped
# systemd unit; Postfix needs the drop-in described in the README.
# [alerts.email]
# to = ["ops@example.org"]
# from = "linkdoctor@example.org"
# sendmail = "/usr/bin/msmtp"

# [[alerts.maintenance]]
# matches = "cuesta"
# from = "2026-11-22T15:00:00Z"
# to = "2026-11-22T19:00:00Z"
# note = "tower work"

# [[alerts.silence]]
# matches = "n0call-lopez"
# reason = "known weak path, waiting on a new dish"
# review = "2027-01-31"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let c: Config = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(c.links.flap_count, 5);
        assert!(!c.weather.on());
        // every commented-out setting switched on
        let all: Vec<&str> = EXAMPLE
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(r)
                    if r.starts_with('[')
                        || r.starts_with('"')
                        || r.split_once(" = ").is_some_and(|(k, _)| !k.contains(' ')) =>
                {
                    r
                }
                _ => l,
            })
            .collect();
        let c: Config = toml::from_str(&all.join(
            "
",
        ))
        .unwrap();
        c.weather.validate().unwrap();
        c.alerts.validate().unwrap();
        assert_eq!(c.weather.station("n0call-cuesta"), Some("KSBP"));
    }
}
