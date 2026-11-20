# Changelog

All notable changes to ridgewarden-linkdoctor are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is what appears on its GitHub release page. Validated in CI by [patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

## [0.1.0] - 2026-11-20

### Added

- Runs as its own system user, `ridgewarden-linkdoctor`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- Slow drift fitted with the Theil-Sen estimator over hourly medians, measured from the last firmware change or radio-change note.
- One-way link alarms, from the quality each end reports and the SNR each end hears.
- Noise floor alarms against the same hour of day over the past month.
- Flapping ("dropped 9 times in the last 24 h") and link loss with the time it happened.
- Single points of failure: "If site Black Mtn fails, North and Paso Robles are cut off from Shack." A What if page for any site or link.
- Next workday list grouped by site and scored as risk times the sites affected.
- Link pages with SNR, quality and noise charts for 7 days, 30 days or a year, with firmware changes and notes marked; `note` command.
- Raw samples kept 30 days, hourly rollups (median, p10, p90) kept 400 days.
- Alerts to ntfy, Discord, webhooks and email with acknowledge links, repeats, quiet hours, maintenance windows and silences; the README covers msmtp and Postfix under the hardened systemd unit.
- Weather cross-check: drift and aim alerts wait for dry weather when the NWS station nearest either end reports rain, rain is forecast for the hour, or the weather app reports a wet-antenna, front or ducting candidate, read from its `weather.candidate` events in the log (or its `/api/events` as a fallback). Rain and candidates are marked on the charts.
- Antenna aim check: a lasting SNR step at both ends with a steady noise floor, while other links at those sites hold, is reported as aim, with the step drawn on the chart.
- Flap counting leaves out drops caused by the polling: partial polls, LQM tracker hiccups the other end contradicts, and the half hour after a firmware change or radio work.
- Upgrade plan page: the order to upgrade firmware so no step cuts this site off from a node still waiting.
- Planner predictions (`planner.prediction` events) on link pages and the workday list: "should be 25 dB, measures 18".
- Track record page: lost links that had a warning first, and problems that cleared after a noted visit.
- Single points of failure ranked by risk, and a three-day forecast for each site on the workday list.

[Unreleased]: https://github.com/Londopy/ridgewarden-linkdoctor/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Londopy/ridgewarden-linkdoctor/releases/tag/v0.1.0
