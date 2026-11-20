# ridgewarden-linkdoctor

[![CI](https://github.com/Londopy/ridgewarden-linkdoctor/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-linkdoctor/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-linkdoctor)](https://github.com/Londopy/ridgewarden-linkdoctor/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-linkdoctor/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-linkdoctor)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**Catch the AREDN link that's slowly dying before it fails.**

A ridgewarden app that keeps long-term history of every link's quality on an AREDN mesh. It warns when a link drifts, flaps or looks knocked off its aim, checks the weather before blaming the hardware, lists the single points of failure, plans firmware upgrades from the far end in, and ranks what to fix on the next site visit, all in plain language.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core running on the same box. The core is the only part that polls the AREDN node and syncs with other sites; this app reads the core's event feed. Spec sheet 07 of the AREDN app suite.

## Features

- Slow drift: "losing about 1.2 dB SNR a week, 5 in all since Nov 2". The trend is fitted with the Theil-Sen estimator over hourly medians, so a rain fade or a few odd readings don't drive it, and it is measured from the last radio change (a firmware update, or a note you add after realigning a dish).
- Antenna aim: a link whose SNR stepped down and stayed down for a day, at both ends where both report, while the noise floor held and the other links at those sites didn't move, is called out as aim (a dish knocked or slipped) rather than interference. The link's SNR chart draws the level before and after the step. A step with a moving noise floor, at one end only, across a whole site, or with no other link to compare is explained as such instead. An aim alert stays open until the link is back within half a step of its old level or someone notes work on it.
- Weather cross-check before drift and aim alerts: rain reported at the NWS station nearest either end, rain forecast for the hour, or a wet-antenna, front or ducting candidate from [ridgewarden-weather](https://github.com/Londopy/ridgewarden-weather) holds the alert until it has been dry for a day. Rain spells and weather app candidates are marked on the charts, and the workday list shows the next three days' forecast for each site.
- One-way links: one end hears the other far better than the other way round, which usually points at a transmitter, cable or connector at one end.
- Noise floor against the same hour of day over the past month, so new interference stands out from the daily cycle.
- Flapping ("dropped 9 times in the last 24 h") and link loss with the time it happened. Drops caused by the polling itself don't count: polls that failed or timed out, a poll in which two or more links vanished together and the next poll listed them again, a missing neighbor the other end listed in its own polls just before and after (an LQM tracker hiccup), and drops in the half hour after a firmware change or radio work. The link page says how many were left out and why. A link is only called lost after two polls in a row without it.
- Single points of failure ranked by risk: the sites each would cut off, weighted by what is already wrong with it. "If site Black Mtn fails, North and Paso Robles are cut off from Shack." A **What if** page shows the same for any site or link you pick.
- **Next workday** list grouped by site: each problem is scored as how bad it is (0 to 3) times one plus the number of sites it affects, and sites are ordered by their total.
- **Upgrade plan**: pick a target firmware and the sites to upgrade, and get the order that never cuts this site off from a node still waiting for its upgrade (the far end first), with what goes out of reach while each one restarts.
- Planner expectations: where [ridgewarden-planner](https://github.com/Londopy/ridgewarden-planner) has published a prediction for a link, the link page reads "should be 25 dB, measures 18", the SNR chart draws the prediction, and a link 10 dB or more short goes on the workday list.
- **Track record**: how many lost links had a warning first, and whether the problems at a site cleared after a visit was noted there, so you can tell whether the app is earning its keep.
- A page per link with SNR, quality and noise charts for 7 days, 30 days or a year, from each end, with firmware changes, notes and rain marked.
- History: raw samples kept 30 days, hourly medians with 10th and 90th percentiles kept 400 days.
- Alerts to ntfy, Discord, any webhook or email, the same way as ridgewarden-health: one per problem, acknowledge links, repeats, quiet hours, maintenance windows and silences with a review date.
- Says "partial picture" when some sites' links aren't visible, rather than guessing. Turn on the core's `[crawl]` to fill in links between sites without an agent.
- `/api/links` returns the same data as JSON.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-linkdoctor/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-linkdoctor/releases/latest): `sudo apt install ./ridgewarden-linkdoctor_<version>_arm64.deb` (`_amd64` on a PC). Either way the download is checked against the release key your core already trusts, `/etc/ridgewarden/linkdoctor.toml` is written if missing, and the service starts once the core is set up. The page is on port 8792.

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-linkdoctor`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-linkdoctor ridgewarden-linkdoctor --config /etc/ridgewarden/linkdoctor.toml <command>`.

## Usage

```sh
ridgewarden-linkdoctor init --config /etc/ridgewarden/linkdoctor.toml
ridgewarden-linkdoctor run  --config /etc/ridgewarden/linkdoctor.toml
```

The pages are on `http://<pi>:8792/`: Links (single points of failure, the next workday list and every link, each with its own page), What if, Upgrade plan and Track record. Add it to the core's dashboard with an `[[app]]` entry in `ridgewarden.toml`.

After a site visit, note what you did, so it shows on that link's charts and the track record can tell whether it fixed things:

```sh
ridgewarden-linkdoctor note n0call-cuesta --to n0call-lopez "realigned the dish" --change
```

`--change` marks a change to the radio path, so the drift trend starts again from there. Notes are signed by your site's core and reach every site.

## Configuration

`/etc/ridgewarden/linkdoctor.toml` (`init` writes a commented one):

```toml
core_url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # only for `note`

[links]
drift_db = 3.0         # SNR lost per week, sustained, before it's flagged
flap_count = 5         # drops in 24 h that count as flapping
one_way_points = 30.0  # quality gap between the two directions
one_way_db = 10.0      # SNR gap between the two ends
noise_rise_db = 6.0    # noise floor above the usual for that hour
aim_step_db = 6.0      # SNR step down that stays a day with a steady noise floor
below_plan_db = 10.0   # this far below the planner's prediction goes on the workday list

[site_names]
"n0call-cuesta" = "Cuesta Peak"

[weather]
user_agent = "ridgewarden-linkdoctor (ops@example.org)"   # turns the NWS check on
feed = true            # the weather app's candidates from the core's log
# app_url = "http://127.0.0.1:8801"   # only if this site's core doesn't carry them
dry_hours = 24         # 1 to 48

[weather.stations]     # only where the nearest NWS station is the wrong one
"n0call-cuesta" = "KSBP"

[alerts]               # on one site only
ntfy = "https://ntfy.sh/your-mesh-topic"
public_url = "http://n0call-pi.local.mesh:8792"
quiet_hours = "22-6"
```

A lost link that cuts sites off is critical; everything else is a warning. Links below the planner's prediction go on the workday list but are not sent as alerts. See ridgewarden-health's README for every alert setting, including maintenance windows and silences.

The app believes only events from stations the core trusts: its own key, plus the keys listed under `[trust]` in `ridgewarden.toml`.

### Weather

With `user_agent` set, the app asks api.weather.gov (which wants the app's name and a contact in that header) for each node's nearest observation station and forecast grid, from the position in the node's polls, once a month. Every 30 minutes it fetches rain reports from those stations, and every hour the forecast for the sites on the workday list and at either end of a drift or aim alert. A station only counts as dry if it has reported in the last 3 hours. While the internet is down the checks fail and the link page says rain wasn't checked; alerts are held on evidence of rain, and otherwise only until each station's first check has been tried, never because a check failed.

The weather app runs at one hub and needs no internet. It publishes each candidate as a signed `weather.candidate` event, so with `feed = true` (the default) every site whose core trusts that hub gets them through the log; nothing else to set. A wet-antenna candidate holds every link at its site, a front or ducting candidate only the links it lists; one withdrawn by a later scan holds nothing, and one never ended is dropped after three days. `app_url` is a fallback for a site whose core doesn't carry the events: it reads the app's `/api/events` every 10 minutes over the mesh.

A held alert stays open if it was already open, and goes out once it has been dry for `dry_hours` at the stations for both ends. The link page shows why it is held and what was checked; the alert text says what was checked when it goes out.

### Email alerts

`[alerts.email]` hands each message to a local sendmail-compatible program:

```toml
[alerts.email]
to = ["ops@example.org"]
from = "linkdoctor@example.org"
sendmail = "/usr/bin/msmtp"
```

The systemd unit runs the app with `ProtectSystem=strict` and `NoNewPrivileges`, so it can't write Postfix's mail spool or use `postdrop`'s setgid. Two ways round that:

- msmtp (`sudo apt install msmtp`) sends straight to your mail server and works with the shipped unit. Put the account in `/etc/msmtprc` and make it readable only by a group for the apps that send mail, not the whole `ridgewarden` group: `sudo groupadd -f mailers && sudo chgrp mailers /etc/msmtprc && sudo chmod 640 /etc/msmtprc && sudo usermod -a -G mailers ridgewarden-linkdoctor`, then restart the service. Leave msmtp's `logfile` unset or use `syslog on`.
- Postfix: let the service into the spool with `sudo systemctl edit ridgewarden-linkdoctor` and

  ```ini
  [Service]
  SupplementaryGroups=postdrop
  ReadWritePaths=/var/spool/postfix/maildrop /var/spool/postfix/public
  ```

  then `sudo systemctl restart ridgewarden-linkdoctor`, and keep `sendmail = "/usr/sbin/sendmail"`.

### Planner predictions

The app reads `planner.prediction` events from the core's log: `{"a": "<node>", "b": "<node>", "snr_db": 25.0, "model": "ITM", "calibrated": true}` (`model` and `calibrated` optional), the newest per link winning.

## Limits

- The planner doesn't publish `planner.prediction` events yet, so expected against measured shows nothing until it does. The event is defined above.
- The NWS check covers the United States only. Elsewhere, use the weather app or leave `[weather]` out.
- In a long wet spell, drift and aim alerts wait until it has been dry for `dry_hours`. Lower it if your links don't mind rain.
- The aim check needs a noise floor reading at the stepped end and at least one other link at either site to compare with; without both, a step shows on the chart but isn't called aim.
- The thresholds come from the simulator and the spec, not from a season of real backbone data; expect to tune `[links]` once you have some.
- Email through Postfix needs the drop-in above. Neither msmtp nor Postfix delivery has been tried on a real Pi from this repo.
- Whether to work with aredn-distributed-monitor rather than compete with it, and whether to feed NewMeshMap, are questions for the people running those projects, not code.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-linkdoctor
cd ridgewarden-linkdoctor
cargo build --release
cargo test
```

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
