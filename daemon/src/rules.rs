//! Rule matching: decide the desired (nice, ionice) for a process.
//!
//! Resolution precedence:
//!   1. An explicit rule matching the process name wins over everything,
//!      including the game-detection heuristic (this is what pins VNyan.exe
//!      to Software despite heavy GPU work).
//!   2. A heuristic-flagged process with no rule gets the `auto_game_default`
//!      preset.
//!   3. Otherwise nothing is applied (default Software behavior).

use std::collections::HashMap;

use nicewatch_common::{AppConfig, AutoGameConfig, CgroupLimits, IoniceClass, Preset, Rule, Tier};

use crate::game_detect::GameFlags;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// No rule and not flagged: leave the process untouched.
    None,
    /// Concrete desired state to apply.
    Apply(ApplyTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyTarget {
    pub tier: Option<Tier>,
    pub nice: i32,
    pub ionice_class: IoniceClass,
    pub ionice_priority: u8,
    /// Source rule key, when a rule produced this (for logging).
    pub rule: Option<String>,
    /// cgroup limits from the rule, if any.
    pub cgroup: Option<CgroupLimits>,
}

#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    /// Rules keyed by process match name.
    by_match: HashMap<String, Rule>,
    pub auto_game: AutoGameConfig,
}

impl RuleSet {
    pub fn from_config(cfg: &AppConfig) -> Self {
        let mut by_match = HashMap::new();
        for rule in cfg.rules.values() {
            by_match.insert(rule.match_name.clone(), rule.clone());
        }
        RuleSet {
            by_match,
            auto_game: cfg.auto_game.clone().unwrap_or_default(),
        }
    }

    pub fn has_rule(&self, process_name: &str) -> bool {
        self.by_match.contains_key(process_name)
    }

    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.by_match.values()
    }

    /// Resolve the desired priority for a process.
    ///
    /// `age_secs` is the process's running time; rules with a `delay` do not
    /// apply until the process has lived that long.
    pub fn resolve(
        &self,
        process_name: &str,
        flags: &GameFlags,
        age_secs: u64,
    ) -> Resolved {
        if let Some(rule) = self.by_match.get(process_name) {
            if let Some(delay) = rule.delay {
                if age_secs < delay {
                    // Short-lived process within its delay window: don't
                    // react yet.
                    return Resolved::None;
                }
            }
            // Explicit rule wins even over the heuristic: nice/ionice fields
            // override the tier preset, so `tier = "software"` really pins a
            // heavy-GPU process to Software.
            let base = rule.tier.unwrap_or(Tier::Software).preset();
            return Resolved::Apply(ApplyTarget {
                tier: rule.tier.or(Some(Tier::Software)),
                nice: rule.nice.unwrap_or(base.nice),
                ionice_class: rule.ionice_class.unwrap_or(base.ionice_class),
                ionice_priority: rule.ionice_priority.unwrap_or(base.ionice_priority),
                rule: Some(rule.name.clone()),
                cgroup: rule.cgroup.clone(),
            });
        }

        if flags.is_game() {
            let p: Preset = self.auto_game.preset();
            let tier = self.auto_game.tier.unwrap_or(Tier::Game);
            return Resolved::Apply(ApplyTarget {
                tier: Some(tier),
                nice: p.nice,
                ionice_class: p.ionice_class,
                ionice_priority: p.ionice_priority,
                rule: None,
                cgroup: None,
            });
        }

        Resolved::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicewatch_common::CgroupLimits;

    fn cfg_with(rules: &[(&str, &str, Option<i64>, Option<u64>)]) -> AppConfig {
        let mut cfg = AppConfig::default();
        for (key, m, nice, delay) in rules {
            cfg.rules.insert(
                key.to_string(),
                Rule {
                    name: key.to_string(),
                    match_name: m.to_string(),
                    tier: Some(Tier::Software),
                    nice: nice.map(|n| n as i32),
                    ionice_class: Some(IoniceClass::BestEffort),
                    ionice_priority: Some(4),
                    delay: *delay,
                    cgroup: None,
                },
            );
        }
        cfg
    }

    fn game_flags() -> GameFlags {
        GameFlags {
            steam_env: true,
            dri_fd: true,
            fullscreen: false,
            known: false,
        }
    }

    #[test]
    fn explicit_rule_wins_over_heuristic() {
        let mut cfg = AppConfig::default();
        // The VNyan case: heavy GPU/DRM user, but pinned to Software.
        cfg.rules.insert(
            "vnyan".into(),
            Rule {
                name: "vnyan".into(),
                match_name: "VNyan.exe".into(),
                tier: Some(Tier::Software),
                nice: Some(0),
                ionice_class: Some(IoniceClass::BestEffort),
                ionice_priority: Some(6),
                delay: None,
                cgroup: None,
            },
        );
        let set = RuleSet::from_config(&cfg);
        let flags = GameFlags {
            steam_env: false,
            dri_fd: true,
            fullscreen: false,
            known: false,
        };
        match set.resolve("VNyan.exe", &flags, 100) {
            Resolved::Apply(t) => {
                assert_eq!(t.tier, Some(Tier::Software));
                assert_eq!(t.nice, 0);
                assert_eq!(t.ionice_priority, 6);
            }
            Resolved::None => panic!("rule must apply"),
        }
    }

    #[test]
    fn heuristic_gets_auto_game_default() {
        let set = RuleSet::from_config(&AppConfig::default());
        match set.resolve("somegame", &game_flags(), 30) {
            Resolved::Apply(t) => {
                assert_eq!(t.tier, Some(Tier::Game));
                assert_eq!(t.nice, Tier::Game.preset().nice);
            }
            _ => panic!("game-flagged process must resolve to the game tier"),
        }
    }

    #[test]
    fn unflagged_new_process_is_left_alone() {
        let set = RuleSet::from_config(&AppConfig::default());
        let flags = GameFlags {
            steam_env: false,
            dri_fd: false,
            fullscreen: false,
            known: false,
        };
        assert_eq!(set.resolve("random-tool", &flags, 30), Resolved::None);
    }

    #[test]
    fn delay_gates_application() {
        let cfg = cfg_with(&[("slow", "slowstart", None, Some(10))]);
        let set = RuleSet::from_config(&cfg);
        assert_eq!(set.resolve("slowstart", &game_flags(), 5), Resolved::None);
        assert!(matches!(
            set.resolve("slowstart", &game_flags(), 15),
            Resolved::Apply(_)
        ));
    }

    #[test]
    fn tier_preset_fills_missing_fields() {
        let mut cfg = AppConfig::default();
        cfg.rules.insert(
            "obs".into(),
            Rule {
                name: "obs".into(),
                match_name: "obs".into(),
                tier: Some(Tier::Streaming),
                nice: None,
                ionice_class: None,
                ionice_priority: None,
                delay: None,
                cgroup: None,
            },
        );
        let set = RuleSet::from_config(&cfg);
        match set.resolve("obs", &game_flags(), 30) {
            Resolved::Apply(t) => {
                let p = Tier::Streaming.preset();
                assert_eq!(t.nice, p.nice);
                assert_eq!(t.ionice_class, p.ionice_class);
                assert_eq!(t.ionice_priority, p.ionice_priority);
            }
            _ => panic!("rule must apply"),
        }
    }

    #[test]
    fn rules_are_keyed_by_match_name() {
        let cfg = cfg_with(&[("key", "procname", None, None)]);
        let set = RuleSet::from_config(&cfg);
        assert!(set.has_rule("procname"));
        assert!(!set.has_rule("procname2"));
    }

    #[test]
    fn cgroup_limits_carry_through() {
        let mut cfg = AppConfig::default();
        cfg.rules.insert(
            "cj".into(),
            Rule {
                name: "cj".into(),
                match_name: "cj".into(),
                tier: Some(Tier::Game),
                nice: None,
                ionice_class: None,
                ionice_priority: None,
                delay: None,
                cgroup: Some(CgroupLimits {
                    path: Some("/sys/fs/cgroup/nicewatch/games".into()),
                    cpu_weight: Some(900),
                    cpu_cap_percent: Some(50),
                    memory_high: Some("4G".into()),
                    memory_max: Some("8G".into()),
                    cpu_idle: None,
                }),
            },
        );
        let set = RuleSet::from_config(&cfg);
        match set.resolve("cj", &game_flags(), 30) {
            Resolved::Apply(t) => {
                let c = t.cgroup.unwrap();
                assert_eq!(c.cpu_weight, Some(900));
                assert_eq!(c.cpu_cap_percent, Some(50));
                assert_eq!(c.memory_high.as_deref(), Some("4G"));
                assert_eq!(c.memory_max.as_deref(), Some("8G"));
                assert_eq!(c.path.as_deref(), Some("/sys/fs/cgroup/nicewatch/games"));
            }
            _ => panic!("rule must apply"),
        }
    }
}