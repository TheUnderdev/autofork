//! Fork moments and trigger matching.
//!
//! Since v0.5 the only moments that fire are idle deadlines and context
//! thresholds — the wake-and-spawn model has no compact/session/boot hooks.
//! Unsupported `run_on` triggers simply never match any moment.

use crate::frontmatter::{ForkDef, ForkRunOn};

/// The context window assumed for `context_used` / `context_left` when the
/// model's real window is unknown. (v0.5 dropped the configurable window; the
/// fork inherits the session's model, whose window we approximate here.)
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// The 1M context window Claude Code marks with a `[1m]` suffix on the
/// session's model id (e.g. `claude-opus-4-8[1m]`).
pub const CONTEXT_WINDOW_1M: u64 = 1_000_000;

/// The context window for a session. A window the client reported explicitly
/// (opencode reads the model's `limit.context` from its catalog, which
/// includes user config overrides) wins outright — the model-id heuristics
/// exist only for clients that report nothing. Claude Code's hook-side model
/// string keeps the `[1m]` marker (the transcript's `message.model` strips
/// it), so marked sessions get the 1M window; Fable/Mythos-family models are
/// 1M unconditionally — their window has no 200k variant, so the bare id is
/// enough. A gauge that already exceeds the resolved window proves it wrong —
/// a heuristic window bumps to the 1M tier (belt for sessions whose events
/// never carried a model) and beyond that to the gauge itself; a reported
/// window saturates at the gauge, so `context_used` never overshoots 100%.
pub fn resolve_context_window(
    model: Option<&str>,
    prompt_tokens: Option<u64>,
    reported: Option<u64>,
) -> u64 {
    let reported = reported.filter(|&w| w > 0);
    let mut window = reported.unwrap_or(match model {
        Some(m) if m.contains("[1m]") || m.contains("fable") || m.contains("mythos") => {
            CONTEXT_WINDOW_1M
        }
        _ => DEFAULT_CONTEXT_WINDOW,
    });
    if let Some(pt) = prompt_tokens {
        if pt > window {
            window = if reported.is_some() {
                pt
            } else {
                CONTEXT_WINDOW_1M.max(pt)
            };
        }
    }
    window
}

/// A fork moment: an event at which rostered forks may fire (matched against
/// each fork's `run_on` config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkMoment {
    /// The session has been idle for exactly this deadline (seconds).
    Idle { deadline_secs: u64 },
    /// End-of-turn context gauge (prompt tokens of the last turn plus the
    /// model's context window, if known).
    Context {
        prompt_tokens: u64,
        max_tokens: Option<u64>,
    },
    /// Wall-clock evaluation point (unix seconds): every poll evaluation
    /// carries one, so `every:` intervals can fire at turn boundaries — and,
    /// on opencode, mid-run via busy polls. `pause_started_at` is `None`
    /// while the session is busy (activity is ongoing); during a pause it
    /// carries the pause's start, and an `every:` trigger whose fork already
    /// ran at or after that instant will not fire again — a quiet session
    /// must not become a periodic cron. The next genuine activity re-arms it.
    Tick {
        now: i64,
        pause_started_at: Option<i64>,
    },
}

/// The base instant an `every:` interval measures from: the fork's last run
/// when it has one, otherwise the session's start.
pub fn every_base(last_run_at: Option<i64>, session_created_at: i64) -> i64 {
    last_run_at.unwrap_or(session_created_at)
}

/// The first `run_on` trigger of `fork` matched by any of `moments`.
/// Forks without an explicit `after_secs` on their idle trigger fire at the
/// default idle deadline (`default_idle_secs`). `every:` triggers measure
/// from `last_run_at` (falling back to `session_created_at`).
pub fn match_moments(
    fork: &ForkDef,
    moments: &[ForkMoment],
    default_idle_secs: u64,
    last_run_at: Option<i64>,
    session_created_at: i64,
) -> Option<ForkRunOn> {
    for moment in moments {
        for trigger in &fork.run_on {
            let hit = match (moment, trigger) {
                (
                    ForkMoment::Tick {
                        now,
                        pause_started_at,
                    },
                    ForkRunOn::Every { interval_secs },
                ) => {
                    let base = every_base(last_run_at, session_created_at);
                    // During a pause, fire only if the fork's last run
                    // predates the pause (there has been activity since);
                    // busy polls (None) always qualify — activity is ongoing.
                    // A fork that never ran always qualifies (the session's
                    // creation was activity): with second-granularity stamps
                    // `created_at == pause_started_at` is common and must not
                    // suppress the first fire, while `ran_at ==
                    // pause_started_at` (a fire at this pause's start) must.
                    let pause_ok = pause_started_at
                        .is_none_or(|pause| last_run_at.is_none_or(|ran| ran < pause));
                    now - base >= *interval_secs as i64 && pause_ok
                }
                (ForkMoment::Idle { deadline_secs }, ForkRunOn::Idle { after_secs }) => {
                    match after_secs {
                        // Explicit deadline: 0 is legal — "at the first Stop
                        // of the pause", the goal-fork recipe.
                        Some(a) => a == deadline_secs,
                        // Bare `idle` uses the configured default, where 0
                        // means disabled.
                        None => default_idle_secs > 0 && default_idle_secs == *deadline_secs,
                    }
                }
                (ForkMoment::Context { prompt_tokens, .. }, ForkRunOn::ContextTokens(n)) => {
                    prompt_tokens >= n
                }
                (
                    ForkMoment::Context {
                        prompt_tokens,
                        max_tokens,
                    },
                    ForkRunOn::ContextUsedPct(p),
                ) => max_tokens.is_some_and(|max| {
                    prompt_tokens.saturating_mul(100) >= max.saturating_mul(*p as u64)
                }),
                (
                    ForkMoment::Context {
                        prompt_tokens,
                        max_tokens,
                    },
                    ForkRunOn::ContextLeft(n),
                ) => max_tokens.is_some_and(|max| max.saturating_sub(*prompt_tokens) <= *n),
                _ => false,
            };
            if hit {
                return Some(*trigger);
            }
        }
    }
    None
}

/// The distinct idle deadlines (seconds, ascending) the given forks need
/// serviced: the default deadline when any fork uses a bare `idle`, plus
/// every explicit `idle: <dur>`. A zero *default* is "disabled" and dropped;
/// an explicit `idle: 0s` is legal and fires at the pause's first Stop.
pub fn idle_deadlines<'a>(
    forks: impl Iterator<Item = &'a ForkDef>,
    default_idle_secs: u64,
) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for fork in forks {
        for trigger in &fork.run_on {
            if let ForkRunOn::Idle { after_secs } = trigger {
                let keep = match after_secs {
                    Some(d) => Some(*d),
                    None if default_idle_secs > 0 => Some(default_idle_secs),
                    None => None,
                };
                if let Some(d) = keep {
                    if !out.contains(&d) {
                        out.push(d);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// The absolute unix instants (ascending, deduped) at which the given forks'
/// `every:` intervals next elapse. `ran_at` looks up a fork's last run by
/// name; forks that never ran measure from `session_created_at`.
pub fn every_fire_times<'a>(
    forks: impl Iterator<Item = (&'a str, &'a ForkDef)>,
    ran_at: impl Fn(&str) -> Option<i64>,
    session_created_at: i64,
) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for (name, fork) in forks {
        for trigger in &fork.run_on {
            if let ForkRunOn::Every { interval_secs } = trigger {
                let fire = every_base(ran_at(name), session_created_at) + *interval_secs as i64;
                if !out.contains(&fire) {
                    out.push(fire);
                }
            }
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::{default_run_on, ForkDef, ForkRunOn};

    fn fork(run_on: Vec<ForkRunOn>) -> ForkDef {
        ForkDef {
            run_on,
            ..ForkDef::default()
        }
    }

    #[test]
    fn default_run_on_matches_default_idle() {
        let f = fork(default_run_on());
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 240 }], 240, None, 0),
            Some(ForkRunOn::Idle { after_secs: None })
        );
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 600 }], 240, None, 0),
            None
        );
    }

    #[test]
    fn explicit_idle_deadline_is_exclusive() {
        let f = fork(vec![ForkRunOn::Idle {
            after_secs: Some(1200),
        }]);
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Idle {
                    deadline_secs: 1200
                }],
                240,
                None,
                0
            ),
            Some(ForkRunOn::Idle {
                after_secs: Some(1200)
            })
        );
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 240 }], 240, None, 0),
            None
        );
    }

    #[test]
    fn zero_default_idle_never_fires() {
        let f = fork(vec![ForkRunOn::Idle { after_secs: None }]);
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 0 }], 0, None, 0),
            None
        );
    }

    #[test]
    fn explicit_zero_idle_fires_at_the_first_stop() {
        // `idle: 0s` — the goal-fork recipe — fires at the pause's first
        // Stop, unlike a zero *default* (which means disabled).
        let f = fork(vec![ForkRunOn::Idle {
            after_secs: Some(0),
        }]);
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 0 }], 240, None, 0),
            Some(ForkRunOn::Idle {
                after_secs: Some(0)
            })
        );
        assert_eq!(idle_deadlines([f].iter(), 240), vec![0]);
    }

    #[test]
    fn unsupported_triggers_never_match() {
        let f = fork(vec![
            ForkRunOn::Compact,
            ForkRunOn::SessionEnd,
            ForkRunOn::Boot,
        ]);
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 240 }], 240, None, 0),
            None
        );
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Context {
                    prompt_tokens: 999_999,
                    max_tokens: Some(200_000)
                }],
                240,
                None,
                0
            ),
            None
        );
    }

    #[test]
    fn context_thresholds() {
        let tokens = fork(vec![ForkRunOn::ContextTokens(100_000)]);
        let used = fork(vec![ForkRunOn::ContextUsedPct(80)]);
        let left = fork(vec![ForkRunOn::ContextLeft(50_000)]);

        let low = [ForkMoment::Context {
            prompt_tokens: 90_000,
            max_tokens: Some(200_000),
        }];
        let high = [ForkMoment::Context {
            prompt_tokens: 170_000,
            max_tokens: Some(200_000),
        }];
        let no_max = [ForkMoment::Context {
            prompt_tokens: 170_000,
            max_tokens: None,
        }];

        assert_eq!(match_moments(&tokens, &low, 240, None, 0), None);
        assert_eq!(
            match_moments(&tokens, &high, 240, None, 0),
            Some(ForkRunOn::ContextTokens(100_000))
        );
        assert_eq!(
            match_moments(&tokens, &no_max, 240, None, 0),
            Some(ForkRunOn::ContextTokens(100_000))
        );

        assert_eq!(match_moments(&used, &low, 240, None, 0), None);
        assert_eq!(
            match_moments(&used, &high, 240, None, 0),
            Some(ForkRunOn::ContextUsedPct(80))
        );
        assert_eq!(match_moments(&used, &no_max, 240, None, 0), None);

        assert_eq!(match_moments(&left, &low, 240, None, 0), None);
        assert_eq!(
            match_moments(&left, &high, 240, None, 0),
            Some(ForkRunOn::ContextLeft(50_000))
        );
        assert_eq!(match_moments(&left, &no_max, 240, None, 0), None);
    }

    #[test]
    fn window_resolution() {
        // No model, small gauge: the default window.
        assert_eq!(resolve_context_window(None, Some(90_000), None), 200_000);
        assert_eq!(
            resolve_context_window(Some("claude-opus-4-8"), None, None),
            200_000
        );
        // The [1m] marker selects the 1M window regardless of gauge.
        assert_eq!(
            resolve_context_window(Some("claude-opus-4-8[1m]"), Some(150_000), None),
            1_000_000
        );
        // Fable/Mythos models are always 1M — no marker needed.
        assert_eq!(
            resolve_context_window(Some("claude-fable-5"), Some(50_000), None),
            1_000_000
        );
        assert_eq!(
            resolve_context_window(Some("claude-mythos-5"), None, None),
            1_000_000
        );
        // A gauge over the assumed window bumps to the 1M tier (model unknown
        // or unmarked), and past 1M the gauge itself becomes the window.
        assert_eq!(resolve_context_window(None, Some(397_929), None), 1_000_000);
        assert_eq!(
            resolve_context_window(Some("claude-opus-4-8"), Some(250_000), None),
            1_000_000
        );
        assert_eq!(
            resolve_context_window(Some("m[1m]"), Some(1_200_000), None),
            1_200_000
        );
    }

    #[test]
    fn reported_window_wins_over_heuristics() {
        // The opencode regression: a bare model id (no [1m], not
        // fable/mythos) with an explicitly reported 1M window must use 1M —
        // before this, the 200k default made `context_used: 75%` fire at
        // 150k (15% of the real window).
        assert_eq!(
            resolve_context_window(Some("claude-sonnet-4-5"), Some(150_000), Some(1_000_000)),
            1_000_000
        );
        // A reported window also wins downward: a genuinely small model must
        // not be assumed 200k.
        assert_eq!(
            resolve_context_window(Some("some-local-model"), Some(10_000), Some(32_000)),
            32_000
        );
        // A gauge over a reported window saturates at the gauge (used never
        // overshoots 100%) without jumping to the 1M tier.
        assert_eq!(
            resolve_context_window(Some("some-local-model"), Some(40_000), Some(32_000)),
            40_000
        );
        // A nonsense zero report falls back to the heuristics.
        assert_eq!(
            resolve_context_window(Some("claude-opus-4-8[1m]"), None, Some(0)),
            1_000_000
        );
    }

    #[test]
    fn used_pct_respects_1m_window() {
        // The exact regression: 75% of a 1M session must not fire at 150k.
        let used = fork(vec![ForkRunOn::ContextUsedPct(75)]);
        let at_150k = [ForkMoment::Context {
            prompt_tokens: 150_000,
            max_tokens: Some(resolve_context_window(
                Some("claude-opus-4-8[1m]"),
                Some(150_000),
                None,
            )),
        }];
        assert_eq!(match_moments(&used, &at_150k, 240, None, 0), None);
        let at_800k = [ForkMoment::Context {
            prompt_tokens: 800_000,
            max_tokens: Some(resolve_context_window(
                Some("claude-opus-4-8[1m]"),
                Some(800_000),
                None,
            )),
        }];
        assert_eq!(
            match_moments(&used, &at_800k, 240, None, 0),
            Some(ForkRunOn::ContextUsedPct(75))
        );
    }

    #[test]
    fn idle_deadline_collection() {
        let forks = [
            fork(vec![ForkRunOn::Idle { after_secs: None }]),
            fork(vec![ForkRunOn::Idle {
                after_secs: Some(1200),
            }]),
            fork(vec![
                ForkRunOn::Idle {
                    after_secs: Some(600),
                },
                ForkRunOn::Compact,
            ]),
            fork(vec![ForkRunOn::Idle {
                after_secs: Some(600),
            }]),
            fork(vec![ForkRunOn::Compact]),
        ];
        assert_eq!(idle_deadlines(forks.iter(), 240), vec![240, 600, 1200]);
        assert_eq!(idle_deadlines(forks.iter(), 0), vec![600, 1200]);
    }

    #[test]
    fn every_matches_on_tick_after_interval() {
        let f = fork(vec![ForkRunOn::Every {
            interval_secs: 3600,
        }]);
        // Never ran: measured from session start.
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Tick {
                    now: 1000 + 3599,
                    pause_started_at: None
                }],
                240,
                None,
                1000
            ),
            None
        );
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Tick {
                    now: 1000 + 3600,
                    pause_started_at: None
                }],
                240,
                None,
                1000
            ),
            Some(ForkRunOn::Every {
                interval_secs: 3600
            })
        );
        // Ran before: measured from the last run, not session start.
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Tick {
                    now: 9000,
                    pause_started_at: None
                }],
                240,
                Some(6000),
                1000
            ),
            None
        );
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Tick {
                    now: 9600,
                    pause_started_at: None
                }],
                240,
                Some(6000),
                1000
            ),
            Some(ForkRunOn::Every {
                interval_secs: 3600
            })
        );
    }

    #[test]
    fn idle_never_matches_a_tick() {
        let f = fork(vec![ForkRunOn::Idle {
            after_secs: Some(1),
        }]);
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Tick {
                    now: 999_999,
                    pause_started_at: None
                }],
                240,
                None,
                0
            ),
            None
        );
    }

    #[test]
    fn every_fire_time_collection() {
        let periodic = fork(vec![ForkRunOn::Every { interval_secs: 100 }]);
        let idle_only = fork(vec![ForkRunOn::Idle { after_secs: None }]);
        let both = fork(vec![
            ForkRunOn::Idle {
                after_secs: Some(60),
            },
            ForkRunOn::Every { interval_secs: 500 },
        ]);
        let forks = [("a", &periodic), ("b", &idle_only), ("c", &both)];
        let ran = |name: &str| (name == "a").then_some(2000i64);
        let times = every_fire_times(forks.iter().map(|&(n, f)| (n, f)), ran, 1000);
        // a: 2000+100; c: never ran, 1000+500. b contributes nothing.
        assert_eq!(times, vec![1500, 2100]);
    }

    #[test]
    fn every_is_gated_by_the_pause() {
        let f = fork(vec![ForkRunOn::Every { interval_secs: 60 }]);
        // Last run before the pause began (activity in between): fires.
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Tick {
                    now: 5000,
                    pause_started_at: Some(4100)
                }],
                240,
                Some(4000),
                0
            ),
            Some(ForkRunOn::Every { interval_secs: 60 })
        );
        // Last run inside the current pause: a quiet session must not become
        // a periodic cron — no re-fire, however much time passes.
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Tick {
                    now: 99_000,
                    pause_started_at: Some(4100)
                }],
                240,
                Some(4200),
                0
            ),
            None
        );
        // Busy poll (no pause): the same fork fires freely mid-run.
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Tick {
                    now: 4300,
                    pause_started_at: None
                }],
                240,
                Some(4200),
                0
            ),
            Some(ForkRunOn::Every { interval_secs: 60 })
        );
    }
}
