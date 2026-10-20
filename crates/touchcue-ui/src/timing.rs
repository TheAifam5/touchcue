//! When prompts appear and disappear, independent of the output backend.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use touchcue_core::{EndReason, RequestId, RequestState};

use crate::{Command, Prompt};

/// Least time a touched prompt stays visible after its hide is requested.
const OUTCOME_HOLD: Duration = Duration::from_millis(600);

/// Change an output backend applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Show(Prompt),
    /// Re-renders a shown prompt in place.
    Update(Prompt),
    Hide(RequestId),
}

#[derive(Debug)]
enum Phase {
    /// Accepted but not shown before `due`.
    Pending { due: Instant },
    /// Shown since `since`; withdrawn at `hide_at` once a hide was requested.
    Visible {
        since: Instant,
        hide_at: Option<Instant>,
    },
}

#[derive(Debug)]
struct Entry {
    prompt: Prompt,
    phase: Phase,
}

/// Applies the show delay and the minimum display time to commands.
///
/// Every method takes the current time, so the schedule is deterministic.
#[derive(Debug)]
pub(crate) struct Timing {
    show_delay: Duration,
    min_display: Duration,
    entries: BTreeMap<RequestId, Entry>,
}

impl Timing {
    pub(crate) fn new(show_delay: Duration, min_display: Duration) -> Self {
        Self {
            show_delay,
            min_display,
            entries: BTreeMap::new(),
        }
    }

    /// Applies a command and returns the actions due at `now`.
    pub(crate) fn command(&mut self, cmd: Command, now: Instant) -> Vec<Action> {
        match cmd {
            Command::Show(prompt) => {
                if let Some(entry) = self.entries.get_mut(&prompt.id) {
                    // A repeated show revives a prompt whose hide is still deferred.
                    if let Phase::Visible { hide_at, .. } = &mut entry.phase {
                        *hide_at = None;
                    }
                    return update(entry, prompt);
                }
                let due = now.checked_add(self.show_delay).unwrap_or(now);
                self.entries.insert(
                    prompt.id,
                    Entry {
                        prompt,
                        phase: Phase::Pending { due },
                    },
                );
            }
            Command::Update(prompt) => {
                if let Some(entry) = self.entries.get_mut(&prompt.id) {
                    return update(entry, prompt);
                }
            }
            Command::Hide(id) => {
                let min_display = self.min_display;
                match self.entries.get_mut(&id) {
                    Some(Entry {
                        phase: Phase::Pending { .. },
                        ..
                    }) => {
                        self.entries.remove(&id);
                    }
                    Some(Entry {
                        prompt,
                        phase: Phase::Visible { since, hide_at },
                    }) => {
                        let shown = since.checked_add(min_display).unwrap_or(*since);
                        let held = if prompt.state == RequestState::Lingering(EndReason::Touched) {
                            now.checked_add(OUTCOME_HOLD).unwrap_or(now)
                        } else {
                            now
                        };
                        *hide_at = Some(shown.max(held));
                    }
                    None => {}
                }
            }
        }
        self.tick(now)
    }

    /// Returns the shows and hides due at `now`.
    pub(crate) fn tick(&mut self, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        self.entries.retain(|id, entry| match entry.phase {
            Phase::Pending { due } if due <= now => {
                entry.phase = Phase::Visible {
                    since: now,
                    hide_at: None,
                };
                actions.push(Action::Show(entry.prompt.clone()));
                true
            }
            Phase::Visible {
                hide_at: Some(at), ..
            } if at <= now => {
                actions.push(Action::Hide(*id));
                false
            }
            Phase::Pending { .. } | Phase::Visible { .. } => true,
        });
        actions
    }

    /// Returns the prompts shown at the moment, in id order.
    pub(crate) fn visible(&self) -> impl Iterator<Item = &Prompt> {
        self.entries
            .values()
            .filter(|entry| matches!(entry.phase, Phase::Visible { .. }))
            .map(|entry| &entry.prompt)
    }

    /// Returns the earliest time at which [`Timing::tick`] has work.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|entry| match entry.phase {
                Phase::Pending { due } => Some(due),
                Phase::Visible { hide_at, .. } => hide_at,
            })
            .min()
    }
}

fn update(entry: &mut Entry, prompt: Prompt) -> Vec<Action> {
    entry.prompt = prompt;
    match entry.phase {
        Phase::Pending { .. } => Vec::new(),
        Phase::Visible { .. } => vec![Action::Update(entry.prompt.clone())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(id: u64, body: &str) -> Prompt {
        Prompt {
            id: RequestId(id),
            title: "Touch".to_owned(),
            body: body.to_owned(),
            icon: None,
            state: RequestState::Waiting,
            language: touchcue_core::Language::English,
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn show_waits_for_the_delay() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(200), ms(0));
        assert_eq!(timing.command(Command::Show(prompt(1, "a")), t0), []);
        assert_eq!(timing.next_deadline(), Some(t0 + ms(200)));
        assert_eq!(timing.tick(t0 + ms(199)), []);
        assert_eq!(timing.tick(t0 + ms(200)), [Action::Show(prompt(1, "a"))]);
        assert_eq!(timing.next_deadline(), None);
    }

    #[test]
    fn zero_delay_shows_at_once() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(0), ms(0));
        assert_eq!(
            timing.command(Command::Show(prompt(1, "a")), t0),
            [Action::Show(prompt(1, "a"))]
        );
        assert_eq!(
            timing.command(Command::Hide(RequestId(1)), t0),
            [Action::Hide(RequestId(1))]
        );
        assert_eq!(timing.next_deadline(), None);
    }

    #[test]
    fn hide_before_shown_shows_nothing() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(300), ms(800));
        timing.command(Command::Show(prompt(1, "a")), t0);
        assert_eq!(
            timing.command(Command::Hide(RequestId(1)), t0 + ms(100)),
            []
        );
        assert_eq!(timing.next_deadline(), None);
        assert_eq!(timing.tick(t0 + ms(1000)), []);
        assert_eq!(
            timing.command(Command::Update(prompt(1, "late")), t0 + ms(1000)),
            []
        );
    }

    #[test]
    fn visible_lists_shown_prompts_only() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(500), ms(0));
        timing.command(Command::Show(prompt(1, "a")), t0);
        timing.command(Command::Show(prompt(2, "b")), t0 + ms(400));
        assert_eq!(timing.visible().count(), 0);
        timing.tick(t0 + ms(500));
        let shown: Vec<_> = timing.visible().map(|prompt| prompt.id).collect();
        assert_eq!(shown, [RequestId(1)]);
        timing.tick(t0 + ms(900));
        let shown: Vec<_> = timing.visible().map(|prompt| prompt.id).collect();
        assert_eq!(shown, [RequestId(1), RequestId(2)]);
    }

    #[test]
    fn hide_waits_for_min_display() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(0), ms(800));
        timing.command(Command::Show(prompt(1, "a")), t0);
        assert_eq!(
            timing.command(Command::Hide(RequestId(1)), t0 + ms(100)),
            []
        );
        assert_eq!(timing.next_deadline(), Some(t0 + ms(800)));
        assert_eq!(timing.tick(t0 + ms(799)), []);
        assert_eq!(timing.tick(t0 + ms(800)), [Action::Hide(RequestId(1))]);
        assert_eq!(timing.next_deadline(), None);

        timing.command(Command::Show(prompt(2, "b")), t0);
        assert_eq!(
            timing.command(Command::Hide(RequestId(2)), t0 + ms(900)),
            [Action::Hide(RequestId(2))]
        );
    }

    #[test]
    fn update_while_visible_rerenders_in_place() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(0), ms(800));
        timing.command(Command::Show(prompt(1, "a")), t0);
        let mut ended = prompt(1, "a");
        ended.state = RequestState::Lingering(EndReason::Cancelled);
        assert_eq!(
            timing.command(Command::Update(ended.clone()), t0 + ms(10)),
            [Action::Update(ended.clone())]
        );
        timing.command(Command::Hide(RequestId(1)), t0 + ms(20));
        assert_eq!(
            timing.command(Command::Update(prompt(1, "b")), t0 + ms(30)),
            [Action::Update(prompt(1, "b"))]
        );
        assert_eq!(timing.tick(t0 + ms(800)), [Action::Hide(RequestId(1))]);
    }

    #[test]
    fn update_while_pending_replaces_the_text() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(100), ms(0));
        timing.command(Command::Show(prompt(1, "a")), t0);
        assert_eq!(timing.command(Command::Update(prompt(1, "b")), t0), []);
        assert_eq!(timing.tick(t0 + ms(100)), [Action::Show(prompt(1, "b"))]);
    }

    #[test]
    fn show_revives_a_deferred_hide() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(0), ms(800));
        timing.command(Command::Show(prompt(1, "a")), t0);
        timing.command(Command::Hide(RequestId(1)), t0 + ms(10));
        assert_eq!(
            timing.command(Command::Show(prompt(1, "b")), t0 + ms(20)),
            [Action::Update(prompt(1, "b"))]
        );
        assert_eq!(timing.next_deadline(), None);
        assert_eq!(timing.tick(t0 + ms(900)), []);
    }

    #[test]
    fn prompts_are_independent() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(100), ms(0));
        timing.command(Command::Show(prompt(1, "a")), t0);
        timing.command(Command::Show(prompt(2, "b")), t0 + ms(50));
        assert_eq!(timing.tick(t0 + ms(100)), [Action::Show(prompt(1, "a"))]);
        assert_eq!(timing.next_deadline(), Some(t0 + ms(150)));
        assert_eq!(timing.tick(t0 + ms(150)), [Action::Show(prompt(2, "b"))]);
    }

    #[test]
    fn touched_prompt_is_held_after_its_hide() {
        let t0 = Instant::now();
        let mut timing = Timing::new(ms(0), ms(100));
        timing.command(Command::Show(prompt(1, "a")), t0);
        let mut touched = prompt(1, "a");
        touched.state = RequestState::Lingering(EndReason::Touched);
        timing.command(Command::Update(touched), t0 + ms(1000));
        assert_eq!(
            timing.command(Command::Hide(RequestId(1)), t0 + ms(1000)),
            []
        );
        assert_eq!(timing.next_deadline(), Some(t0 + ms(1000) + OUTCOME_HOLD));
        assert_eq!(
            timing.tick(t0 + ms(1000) + OUTCOME_HOLD),
            [Action::Hide(RequestId(1))]
        );
    }
}
