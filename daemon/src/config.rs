//! Parsing / validation / rendering of `rules.toml` files.

use nicewatch_common::AppConfig;

/// Parse and validate a rules file.  Reported errors are human-readable
/// TOML problems; invalid *values* (out-of-range nice etc.) are collected
/// into the returned warnings but do not fail the whole file.
pub fn parse(input: &str) -> Result<AppConfig, String> {
    let mut cfg: AppConfig = toml::from_str(input).map_err(|e| format!("TOML parse error: {e}"))?;
    cfg.fill_rule_names();
    for w in validate(&cfg) {
        log::warn!("{w}");
    }
    Ok(cfg)
}

pub fn render(cfg: &AppConfig) -> String {
    toml::to_string_pretty(cfg).expect("config must serialize to TOML")
}

pub fn validate(cfg: &AppConfig) -> Vec<String> {
    let mut out = Vec::new();
    for (key, rule) in &cfg.rules {
        if rule.match_name.is_empty() {
            out.push(format!("rule [rules.{key}]: `match` must not be empty"));
        }
        if let Some(n) = rule.nice {
            if !(-20..=19).contains(&n) {
                out.push(format!(
                    "rule [rules.{key}]: nice {n} out of range (-20..=19), ignoring"
                ));
            }
        }
        if let Some(p) = rule.ionice_priority {
            if p > 7 {
                out.push(format!(
                    "rule [rules.{key}]: ionice_priority {p} out of range (0..=7), ignoring"
                ));
            }
        }
        if let Some(w) = rule.cgroup.as_ref().and_then(|c| c.cpu_weight) {
            if !(1..=10000).contains(&w) {
                out.push(format!(
                    "rule [rules.{key}]: cgroup cpu_weight {w} out of range (1..=10000), ignoring"
                ));
            }
        }
        if let Some(p) = rule.cgroup.as_ref().and_then(|c| c.cpu_cap_percent) {
            if !(1..=3200).contains(&p) {
                out.push(format!(
                    "rule [rules.{key}]: cgroup cpu_cap_percent {p} out of range (1..=3200, \
                     percent of ONE core), ignoring"
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicewatch_common::{IoniceClass, Tier};

    #[test]
    fn parses_example_shape() {
        let toml = r#"
            [rules.firefox]
            match = "firefox-bin"
            tier = "software"
            nice = 5
            ionice_class = "best-effort"
            ionice_priority = 6

            [rules.vnyan]
            match = "VNyan.exe"
            tier = "software"
            nice = 0

            [auto_game_default]
            tier = "game"
            nice = -8
            ionice_class = "best-effort"
            ionice_priority = 2
        "#;
        let cfg = parse(toml).unwrap();
        assert_eq!(cfg.rules.len(), 2);
        let fx = &cfg.rules["firefox"];
        assert_eq!(fx.match_name, "firefox-bin");
        assert_eq!(fx.tier, Some(Tier::Software));
        assert_eq!(fx.nice, Some(5));
        assert_eq!(fx.ionice_class, Some(IoniceClass::BestEffort));
        assert_eq!(fx.ionice_priority, Some(6));
        let auto = cfg.auto_game.unwrap();
        assert_eq!(auto.tier, Some(Tier::Game));
        assert_eq!(auto.preset().nice, -8);
    }

    #[test]
    fn out_of_range_nice_is_warning_not_fatal() {
        let cfg = r#"
            [rules.bad]
            match = "bad"
            nice = 99
        "#;
        let parsed = parse(cfg).unwrap();
        assert_eq!(parsed.rules["bad"].nice, Some(99));
        let warns = validate(&parsed);
        assert!(!warns.is_empty());
    }

    #[test]
    fn bad_toml_is_a_hard_error() {
        let cfg = r#"
            [rules.
        "#;
        assert!(parse(cfg).is_err());
    }

    #[test]
    fn round_trip_preserves_rules() {
        let cfg = parse(
            r#"
            poll_interval_ms = 3000

            [rules.vnyan]
            match = "VNyan.exe"
            tier = "software"
            nice = 0
            ionice_class = "best-effort"
            ionice_priority = 6
        "#,
        )
        .unwrap();
        let text = render(&cfg);
        let again = parse(&text).unwrap();
        assert_eq!(again, cfg);
        assert_eq!(again.poll_interval_ms, Some(3000));
        assert_eq!(again.rules["vnyan"].tier, Some(Tier::Software));
    }
}