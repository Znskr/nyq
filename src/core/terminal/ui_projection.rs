use super::contracts::{
    ActionRequest, ActionResult, Attention, AttentionState, Backend, Lifecycle, ResponseResult,
    SessionFacts, ShellSpec,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub session_id: Vec<u8>,
    pub label: Vec<u8>,
    pub lifecycle: Lifecycle,
    pub attention: AttentionState,
    pub backend: Backend,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub rows: Vec<Row>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BackgroundStatus {
    pub count: usize,
    pub running: bool,
}

pub fn background_status(rows: &[Row]) -> BackgroundStatus {
    let mut result = BackgroundStatus::default();
    for row in rows {
        if row.lifecycle != Lifecycle::Running
            || row.attention.attention != Attention::Background
        {
            continue;
        }
        result.count += 1;
        result.running = true;
    }
    result
}

#[derive(Clone, Debug, Default)]
pub struct Store {
    rows: Vec<Row>,
}

impl Store {
    pub fn observe(&mut self, request: &ActionRequest, result: &ResponseResult) {
        let success = match result {
            ResponseResult::Success(value) => value,
            ResponseResult::Failure(_) => return,
        };
        match success {
            ActionResult::List(value) => match request {
                ActionRequest::List(filters)
                    if filters.task_id.is_none()
                        && filters.workspace_root.is_none()
                        && filters.lifecycle.is_none()
                        && filters.backend.is_none() =>
                {
                    self.replace_all(&value.sessions);
                }
                _ => {
                    for facts in &value.sessions {
                        self.upsert(facts, None);
                    }
                }
            },
            ActionResult::Inspect(value) => {
                self.upsert(&value.session, Some(value.command.as_deref().unwrap_or(&value.shell)));
            }
            ActionResult::Start(value) => self.upsert(&value.session, label_from_request(request)),
            ActionResult::Read(value) => self.upsert(&value.session, None),
            ActionResult::Screen(value) => self.upsert(&value.session, None),
            ActionResult::Write(value) => self.upsert(&value.session, None),
            ActionResult::Wait(value) => self.upsert(&value.session, None),
            ActionResult::Monitor(value) => self.upsert(&value.session, None),
            ActionResult::Resize(value) => self.upsert(&value.session, None),
            ActionResult::Signal(value) => self.upsert(&value.session, None),
            ActionResult::Close(value) => self.upsert(&value.session, None),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot { rows: self.rows.clone() }
    }

    pub fn clear(&mut self) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        self.rows.clear();
        true
    }

    fn replace_all(&mut self, sessions: &[SessionFacts]) {
        let mut replacement = Vec::with_capacity(sessions.len());
        for facts in sessions {
            let label = self
                .rows
                .iter()
                .find(|row| row.session_id == facts.session_id)
                .map(|row| row.label.clone());
            replacement.push(make_row(facts, label.as_deref()));
        }
        self.rows = replacement;
    }

    fn upsert(&mut self, facts: &SessionFacts, label: Option<&[u8]>) {
        for row in self.rows.iter_mut() {
            if row.session_id != facts.session_id {
                continue;
            }
            if let Some(value) = label {
                row.label = value.to_vec();
            }
            row.lifecycle = facts.lifecycle;
            row.attention = facts.attention;
            row.backend = facts.backend;
            return;
        }
        self.rows.push(make_row(facts, label));
    }
}

fn make_row(facts: &SessionFacts, label: Option<&[u8]>) -> Row {
    let display = label.unwrap_or(facts.session_id.as_slice());
    Row {
        session_id: facts.session_id.clone(),
        label: display.to_vec(),
        lifecycle: facts.lifecycle,
        attention: facts.attention,
        backend: facts.backend,
    }
}

fn label_from_request(request: &ActionRequest) -> Option<&[u8]> {
    match request {
        ActionRequest::Start(value) => {
            let fallback = match &value.shell {
                ShellSpec::UserLogin => b"interactive shell".as_slice(),
                ShellSpec::Executable { path, .. } => path.as_slice(),
            };
            Some(value.command.as_deref().unwrap_or(fallback))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::terminal::contracts::{
        AllowedControls, ListFilters, ListResult, PersistenceLevel, RawCursor, ScreenRecovery,
        ScreenUnavailableReason, WriteLease,
    };

    fn test_facts(session_id: &[u8], lifecycle: Lifecycle) -> SessionFacts {
        SessionFacts {
            session_id: session_id.to_vec(),
            lifecycle,
            attention: AttentionState::default(),
            backend: Backend::Native,
            persistence: PersistenceLevel::Durable,
            output_cursor: RawCursor { segment: 1, offset: 0 },
            unread_range: None,
            raw_gap: None,
            screen_recovery: ScreenRecovery::Unavailable(ScreenUnavailableReason::Missing),
            active_monitor_count: 0,
            next_actions: AllowedControls::default(),
        }
    }

    #[test]
    fn background_status_counts_only_running_background_terminal_sessions() {
        let rows = [
            Row {
                session_id: b"a".to_vec(),
                label: b"a".to_vec(),
                lifecycle: Lifecycle::Running,
                attention: AttentionState::default(),
                backend: Backend::Native,
            },
            Row {
                session_id: b"b".to_vec(),
                label: b"b".to_vec(),
                lifecycle: Lifecycle::Running,
                attention: AttentionState { attention: Attention::AgentWait, write_lease: WriteLease::Agent },
                backend: Backend::Native,
            },
            Row {
                session_id: b"c".to_vec(),
                label: b"c".to_vec(),
                lifecycle: Lifecycle::Exited,
                attention: AttentionState::default(),
                backend: Backend::Tmux,
            },
            Row {
                session_id: b"d".to_vec(),
                label: b"d".to_vec(),
                lifecycle: Lifecycle::Running,
                attention: AttentionState { attention: Attention::UserTakeover, write_lease: WriteLease::Human },
                backend: Backend::Native,
            },
            Row {
                session_id: b"e".to_vec(),
                label: b"e".to_vec(),
                lifecycle: Lifecycle::Starting,
                attention: AttentionState::default(),
                backend: Backend::Native,
            },
            Row {
                session_id: b"f".to_vec(),
                label: b"f".to_vec(),
                lifecycle: Lifecycle::Closed,
                attention: AttentionState::default(),
                backend: Backend::Tmux,
            },
        ];
        let status = background_status(&rows);
        assert_eq!(1, status.count);
        assert!(status.running);
    }

    #[test]
    fn filtered_catalog_observations_update_rows_without_replacing_the_full_projection() {
        let mut store = Store::default();
        let full = [
            test_facts(b"terminal-a", Lifecycle::Running),
            test_facts(b"terminal-b", Lifecycle::Starting),
        ];
        store.observe(
            &ActionRequest::List(ListFilters::default()),
            &ResponseResult::Success(ActionResult::List(ListResult { sessions: full.to_vec() })),
        );

        let partial = [test_facts(b"terminal-b", Lifecycle::Running)];
        store.observe(
            &ActionRequest::List(ListFilters {
                lifecycle: Some(Lifecycle::Running),
                ..ListFilters::default()
            }),
            &ResponseResult::Success(ActionResult::List(ListResult {
                sessions: partial.to_vec(),
            })),
        );
        let snapshot = store.snapshot();
        assert_eq!(2, snapshot.rows.len());
        assert_eq!(Lifecycle::Running, snapshot.rows[1].lifecycle);
    }
}
