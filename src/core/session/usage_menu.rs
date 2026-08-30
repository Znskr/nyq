//! Interactive usage-menu state machine.
//!
//! Mirrors `zig-reference/src/core/session/usage_menu.zig`. The menu owns its
//! snapshot and preserves model selection/expansion by model id across
//! refreshes.

use super::usage_report::{ModelUsage, Scope, Snapshot};

#[derive(Default, Debug)]
pub struct State {
    pub active: bool,
    pub requested_scope: Scope,
    pub selected_model: usize,
    pub expanded_model: Option<usize>,
    pub model_window_start: usize,
    pub snapshot: Option<Snapshot>,
    pub refresh_error: Option<Vec<u8>>,
}

impl State {
    /// Takes ownership of `snapshot`; the caller must not deinitialize it.
    pub fn open_owned(&mut self, snapshot: Snapshot) {
        self.close();
        *self = State {
            active: true,
            requested_scope: snapshot.scope,
            snapshot: Some(snapshot),
            ..State::default()
        };
    }

    pub fn open_error(&mut self, scope_value: Scope, message: &[u8]) {
        self.close();
        *self = State {
            active: true,
            requested_scope: scope_value,
            refresh_error: Some(message.to_vec()),
            ..State::default()
        };
    }

    pub fn open_loading(&mut self, scope_value: Scope) {
        self.close();
        *self = State {
            active: true,
            requested_scope: scope_value,
            ..State::default()
        };
    }

    pub fn set_loading_scope(&mut self, scope_value: Scope) {
        self.snapshot = None;
        self.refresh_error = None;
        self.requested_scope = scope_value;
        self.selected_model = 0;
        self.expanded_model = None;
        self.model_window_start = 0;
    }

    pub fn close(&mut self) {
        *self = State::default();
    }

    /// Replaces the frozen snapshot while retaining selection and expansion by
    /// model identifier when that model still exists.
    pub fn replace_owned(&mut self, snapshot: Snapshot) {
        let old_selected = self.snapshot.as_ref().and_then(|old| {
            if old.models.is_empty() {
                None
            } else {
                Some(old.models[self.selected_model.min(old.models.len() - 1)].model.clone())
            }
        });
        let old_expanded = self.snapshot.as_ref().and_then(|old| {
            self.expanded_model.and_then(|index| {
                if index < old.models.len() {
                    Some(old.models[index].model.clone())
                } else {
                    None
                }
            })
        });

        let selected = find_model(&snapshot.models, old_selected.as_deref()).unwrap_or_else(|| {
            self.selected_model
                .min(snapshot.models.len().saturating_sub(1))
        });
        let expanded = find_model(&snapshot.models, old_expanded.as_deref());
        self.snapshot = Some(snapshot);
        self.requested_scope = self.snapshot.as_ref().unwrap().scope;
        self.selected_model = selected;
        self.expanded_model = expanded;
        self.refresh_error = None;
        self.model_window_start = self.model_window_start.min(selected);
    }

    pub fn record_refresh_failure(&mut self, attempted_scope: Scope, message: &[u8]) {
        self.refresh_error = Some(message.to_vec());
        self.requested_scope = attempted_scope;
    }

    pub fn move_model(&mut self, delta: i32, visible_model_rows: usize) -> bool {
        if !self.active || delta == 0 {
            return false;
        }
        let snapshot = match &self.snapshot {
            Some(snapshot) => snapshot,
            None => return false,
        };
        if snapshot.models.is_empty() {
            return false;
        }
        let next = if delta > 0 {
            self.selected_model
                .saturating_add(1)
                .min(snapshot.models.len() - 1)
        } else {
            self.selected_model.saturating_sub(1)
        };
        if next == self.selected_model {
            return false;
        }
        self.selected_model = next;
        self.keep_selection_visible(visible_model_rows);
        true
    }

    pub fn toggle_expanded(&mut self, visible_model_rows: usize) -> bool {
        if !self.active {
            return false;
        }
        let snapshot = match &self.snapshot {
            Some(snapshot) => snapshot,
            None => return false,
        };
        if snapshot.models.is_empty() {
            return false;
        }
        let selected = self.selected_model.min(snapshot.models.len() - 1);
        self.expanded_model = if self.expanded_model == Some(selected) {
            None
        } else {
            Some(selected)
        };
        self.keep_selection_visible(visible_model_rows);
        true
    }

    pub fn scope(&self) -> Scope {
        match &self.snapshot {
            Some(snapshot) => snapshot.scope,
            None => self.requested_scope,
        }
    }

    pub fn navigation_scope(&self) -> Scope {
        self.requested_scope
    }

    fn keep_selection_visible(&mut self, visible_model_rows: usize) {
        let visible = visible_model_rows.max(1);
        if self.selected_model < self.model_window_start {
            self.model_window_start = self.selected_model;
        } else if self.selected_model >= self.model_window_start.saturating_add(visible) {
            self.model_window_start = self.selected_model - (visible - 1);
        }
    }
}

fn find_model(models: &[ModelUsage], target: Option<&[u8]>) -> Option<usize> {
    let name = target?;
    models.iter().position(|model| model.model == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::usage_report::{self, Totals};

    fn test_totals(tokens: u64) -> Totals {
        Totals {
            total_tokens: tokens,
            input_tokens: tokens,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            request_count: Some(1),
            total_cost: 0.0,
        }
    }

    fn model(name: &str, tokens: u64) -> ModelUsage {
        ModelUsage {
            model: name.as_bytes().to_vec(),
            totals: test_totals(tokens),
        }
    }

    fn snapshot(scope: Scope, models: Vec<ModelUsage>) -> Snapshot {
        Snapshot {
            scope,
            snapshot_time_ms: 100,
            window_start_ms: 0,
            coverage_started_at_ms: Some(0),
            coverage: usage_report::Coverage::Full,
            completeness: usage_report::Completeness::Complete,
            totals: Some(test_totals(15)),
            models,
            session_activity: None,
        }
    }

    #[test]
    fn usage_menu_owns_snapshots_and_preserves_model_identity_on_refresh() {
        let menu_snapshot = snapshot(
            Scope::Days30,
            vec![model("provider/a", 10), model("provider/b", 5)],
        );
        let mut menu = State::default();
        menu.open_owned(menu_snapshot);

        assert!(menu.active);
        assert!(menu.move_model(1, 16));
        assert!(menu.toggle_expanded(16));

        menu.replace_owned(snapshot(
            Scope::Hours24,
            vec![model("provider/b", 20), model("provider/c", 1)],
        ));
        assert_eq!(0, menu.selected_model);
        assert_eq!(Some(0), menu.expanded_model);
        assert_eq!(Scope::Hours24, menu.scope());
    }

    #[test]
    fn usage_menu_advances_failed_scopes_while_retaining_its_last_snapshot() {
        let mut menu = State::default();

        menu.open_error(Scope::Days30, b"unavailable");
        menu.record_refresh_failure(Scope::Days7, b"unavailable");
        assert_eq!(Scope::Days7, menu.scope());
        assert_eq!(Scope::Days7, menu.navigation_scope());
        menu.record_refresh_failure(Scope::Hours24, b"unavailable");
        assert_eq!(Scope::Hours24, menu.scope());
        assert_eq!(Scope::Hours24, menu.navigation_scope());

        menu.replace_owned(snapshot(Scope::Days30, vec![]));
        menu.record_refresh_failure(menu.navigation_scope().previous(), b"unavailable");
        assert_eq!(Scope::Days30, menu.scope());
        assert_eq!(Scope::Days7, menu.navigation_scope());
        menu.record_refresh_failure(menu.navigation_scope().previous(), b"unavailable");
        assert_eq!(Scope::Days30, menu.scope());
        assert_eq!(Scope::Hours24, menu.navigation_scope());
        let session_scope = menu.navigation_scope().previous();
        assert_eq!(Scope::Session, session_scope);
        menu.replace_owned(snapshot(Scope::Session, vec![]));
        assert_eq!(Scope::Session, menu.scope());
        assert_eq!(Scope::Session, menu.navigation_scope());
    }

    #[test]
    fn usage_menu_can_publish_a_loading_scope_before_data_arrives() {
        let mut menu = State::default();

        menu.open_loading(Scope::Days30);
        assert!(menu.active);
        assert_eq!(Scope::Days30, menu.navigation_scope());
        assert!(menu.snapshot.is_none());
        assert!(menu.refresh_error.is_none());

        menu.set_loading_scope(Scope::Days7);
        assert_eq!(Scope::Days7, menu.navigation_scope());
    }

    #[test]
    fn usage_menu_keeps_session_model_selection_inside_the_rendered_viewport() {
        let models: Vec<ModelUsage> = (0..13)
            .map(|index| model(&format!("provider/model-{index}"), index as u64 + 1))
            .collect();
        let mut menu_snapshot = snapshot(Scope::Session, models);
        menu_snapshot.session_activity = Some(usage_report::SessionActivity {
            api_duration_complete: true,
            wall_duration_complete: true,
            code_complete: true,
            api_duration_ms: 1,
            wall_duration_ms: 2,
            lines_added: 3,
            lines_removed: 4,
        });

        let mut menu = State::default();
        menu.open_owned(menu_snapshot);

        for _ in 0..12 {
            assert!(menu.move_model(1, 32));
        }
        assert_eq!(12, menu.selected_model);
        assert_eq!(0, menu.model_window_start);

        assert!(menu.toggle_expanded(32));
        assert_eq!(0, menu.model_window_start);
        assert!(menu.move_model(-1, 32));
        assert_eq!(11, menu.selected_model);
        assert_eq!(0, menu.model_window_start);
    }
}
