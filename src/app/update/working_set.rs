//! Process-global working-set trimming (Windows only).
//!
//! `EmptyWorkingSet` returns the whole process's resident pages to the OS, so
//! it applies only when the entire app is backgrounded, never per window. A
//! state change re-evaluates the condition across all windows: first hold arms
//! a timer, a break bumps the generation to cancel it, and the trim re-checks
//! the condition before firing, so a refocus in the grace period wins.

use iced::Task;

use crate::app::{App, Envelope};
use crate::config::WorkingSetTrim;

/// Re-evaluate the trim condition after a state change and arm or cancel the
/// timer on a transition. A no-op while the condition is unchanged, so the
/// periodic minimize poll never resets a running timer.
pub(crate) fn reconcile(app: &mut App) -> Task<Envelope> {
    let cfg = app.shared.config.advanced.resource.working_set;
    let met = condition_met(app, cfg.trim_when);
    if met == app.shared.working_set.armed {
        return Task::none();
    }
    app.shared.working_set.armed = met;
    // Any pending timer carries the old generation, so bumping it here cancels a
    // trim that a refocus or restore invalidated.
    app.shared.working_set.generation += 1;
    if !met {
        return Task::none();
    }
    let generation = app.shared.working_set.generation;
    let after = cfg.trim_after;
    Task::future(async move {
        tokio::time::sleep(after).await;
        Envelope::TrimWorkingSet(generation)
    })
}

/// Fire the trim if its timer is still the current one and the condition still
/// holds. A superseded generation, or a window that came back, no-ops.
pub(crate) fn on_timer(app: &mut App, generation: u64) -> Task<Envelope> {
    let trim_when = app.shared.config.advanced.resource.working_set.trim_when;
    if generation == app.shared.working_set.generation && condition_met(app, trim_when) {
        crate::platform::trim_working_set();
    }
    Task::none()
}

/// Whether the configured background condition holds across every open window.
fn condition_met(app: &App, trim_when: WorkingSetTrim) -> bool {
    if app.windows.is_empty() {
        return false;
    }
    match trim_when {
        WorkingSetTrim::Never => false,
        WorkingSetTrim::AllUnfocused => app.windows.values().all(|w| !w.focused),
        WorkingSetTrim::AllMinimized => app.windows.values().all(|w| w.minimized),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{empty_app, into_app};

    #[test]
    fn all_unfocused_tracks_window_focus() {
        let (mut app, id) = into_app(empty_app());
        // A fresh window starts focused, so the app is not in the background.
        assert!(!condition_met(&app, WorkingSetTrim::AllUnfocused));
        app.windows.get_mut(&id).unwrap().focused = false;
        assert!(condition_met(&app, WorkingSetTrim::AllUnfocused));
    }

    #[test]
    fn all_minimized_tracks_minimize() {
        let (mut app, id) = into_app(empty_app());
        assert!(!condition_met(&app, WorkingSetTrim::AllMinimized));
        // Unfocused alone is not enough for the minimized measure.
        app.windows.get_mut(&id).unwrap().focused = false;
        assert!(!condition_met(&app, WorkingSetTrim::AllMinimized));
        app.windows.get_mut(&id).unwrap().minimized = true;
        assert!(condition_met(&app, WorkingSetTrim::AllMinimized));
    }

    #[test]
    fn never_is_never_met() {
        let (mut app, id) = into_app(empty_app());
        app.windows.get_mut(&id).unwrap().focused = false;
        app.windows.get_mut(&id).unwrap().minimized = true;
        assert!(!condition_met(&app, WorkingSetTrim::Never));
    }

    #[test]
    fn reconcile_arms_on_transition_then_stays_put_on_a_poll() {
        let (mut app, id) = into_app(empty_app());
        app.shared.config.advanced.resource.working_set.trim_when = WorkingSetTrim::AllUnfocused;

        // Focused: nothing armed.
        let _ = reconcile(&mut app);
        assert!(!app.shared.working_set.armed);
        let armed_gen = {
            // Unfocus: the transition arms and bumps the generation.
            app.windows.get_mut(&id).unwrap().focused = false;
            let _ = reconcile(&mut app);
            assert!(app.shared.working_set.armed);
            app.shared.working_set.generation
        };

        // Still unfocused: no transition, so a repeat (the minimize poll) leaves
        // the generation alone and the running timer intact.
        let _ = reconcile(&mut app);
        assert_eq!(app.shared.working_set.generation, armed_gen);

        // Refocus: the transition cancels by bumping the generation again.
        app.windows.get_mut(&id).unwrap().focused = true;
        let _ = reconcile(&mut app);
        assert!(!app.shared.working_set.armed);
        assert_eq!(app.shared.working_set.generation, armed_gen + 1);
    }
}
