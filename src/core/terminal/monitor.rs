use super::contracts::{
    self, CheckpointChecksum, MonitorCondition, MonitorDefinition, MonitorEventReason,
    MonitorLifetime, MonitorState, NotifySchedule, PollSchedule,
};

pub const schema_version: u16 = 1;
pub const minimum_schedule_ms: u64 = 10;
pub const maximum_schedule_ms: u64 = 24 * 60 * 60 * 1000;
pub const maximum_lifetime_ms: u64 = 365 * 24 * 60 * 60 * 1000;
pub const maximum_pattern_bytes: usize = 256;
pub const pattern_word_count: usize = (maximum_pattern_bytes + 1 + 63) / 64;
pub const probe_timeout_ms: usize = 2_000;
pub const probe_output_bytes: usize = 16 * 1024;

pub type EventReason = MonitorEventReason;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonitorError {
    InvalidSchedule,
    InvalidMonitorCondition,
    InvalidMonitorLifetime,
    DeadlineOverflow,
    MonitorIdExhausted,
    CounterOverflow,
    InvalidMonitorState,
    InvalidEventId,
    InvalidPattern,
    InvalidClock,
}

impl From<contracts::Error> for MonitorError {
    fn from(error: contracts::Error) -> Self {
        match error {
            contracts::Error::InvalidMonitorCondition => MonitorError::InvalidMonitorCondition,
            contracts::Error::InvalidMonitorLifetime => MonitorError::InvalidMonitorLifetime,
            contracts::Error::InvalidSchedule
            | contracts::Error::MissingCheckSchedule
            | contracts::Error::UnexpectedCheckSchedule => MonitorError::InvalidSchedule,
            _ => MonitorError::InvalidMonitorState,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PathBaseline {
    pub exists: bool,
    pub size: u64,
    pub modified_ns: i128,
}

#[derive(Clone, Copy, Debug)]
pub struct Runtime {
    pub state: MonitorState,
    pub generation: u64,
    pub created_at_ms: i64,
    pub lifetime_deadline_ms: Option<i64>,
    pub next_check_ms: Option<i64>,
    pub next_notification_ms: Option<i64>,
    pub check_count: u64,
    pub notification_count: u64,
    pub last_event_id: u64,
    pub last_event_reason: Option<EventReason>,
    pub condition_matched: bool,
    pub matcher_states: [u64; pattern_word_count],
    pub path_baseline: Option<PathBaseline>,
    pub probe_cwd_fingerprint: Option<CheckpointChecksum>,
}

#[derive(Clone, Debug)]
pub struct PersistedMonitor {
    pub monitor_id: Vec<u8>,
    pub definition: MonitorDefinition,
    pub runtime: Runtime,
}

#[derive(Clone, Debug)]
pub struct PersistedSet {
    pub schema_version: u16,
    pub next_monitor_id: u64,
    pub monitors: Vec<PersistedMonitor>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Decision {
    pub notify: Option<EventReason>,
    pub remove: bool,
    pub state_changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Observation { Check, Output, Screen, Quiet, Exit, SessionExit }

pub fn validate_definition(definition: &MonitorDefinition) -> Result<(), MonitorError> {
    definition.validate()?;
    if let Some(schedule) = definition.check_schedule {
        validate_schedule(schedule)?;
    }
    match definition.notify_schedule {
        NotifySchedule::Interval(schedule) => validate_schedule(schedule)?,
        NotifySchedule::EveryNChecks(count) => {
            if count > 1_000_000 {
                return Err(MonitorError::InvalidSchedule);
            }
        }
        _ => {}
    }
    match definition.lifetime {
        MonitorLifetime::DurationMs(duration_ms) => {
            if duration_ms > maximum_lifetime_ms {
                return Err(MonitorError::InvalidMonitorLifetime);
            }
        }
        _ => {}
    }
    match &definition.condition {
        MonitorCondition::OutputContains(pattern)
        | MonitorCondition::OutputMatches(pattern)
        | MonitorCondition::ScreenMatches(pattern) => {
            if pattern.len() > maximum_pattern_bytes {
                return Err(MonitorError::InvalidMonitorCondition);
            }
        }
        MonitorCondition::OutputQuietMs(duration_ms) => {
            if *duration_ms < minimum_schedule_ms || *duration_ms > maximum_schedule_ms {
                return Err(MonitorError::InvalidMonitorCondition);
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_schedule(schedule: PollSchedule) -> Result<(), MonitorError> {
    if schedule.interval_ms < minimum_schedule_ms || schedule.interval_ms > maximum_schedule_ms {
        return Err(MonitorError::InvalidSchedule);
    }
    Ok(())
}

pub fn deadline(now_ms: i64, duration_ms: u64) -> Result<i64, MonitorError> {
    let duration = i64::try_from(duration_ms).map_err(|_| MonitorError::DeadlineOverflow)?;
    now_ms.checked_add(duration).ok_or(MonitorError::DeadlineOverflow)
}

pub fn initial_runtime(definition: MonitorDefinition, now_ms: i64) -> Result<Runtime, MonitorError> {
    validate_definition(&definition)?;
    let lifetime_deadline_ms = match definition.lifetime {
        MonitorLifetime::DurationMs(duration_ms) => Some(deadline(now_ms, duration_ms)?),
        MonitorLifetime::UntilMatch | MonitorLifetime::UntilSessionEnd => None,
    };
    let next_check_ms = match definition.check_schedule {
        Some(schedule) => Some(deadline(now_ms, schedule.interval_ms)?),
        None => match definition.condition {
            MonitorCondition::OutputQuietMs(duration_ms) => Some(deadline(now_ms, duration_ms)?),
            _ => None,
        },
    };
    let next_notification_ms = match definition.notify_schedule {
        NotifySchedule::Interval(schedule) => Some(deadline(now_ms, schedule.interval_ms)?),
        _ => None,
    };
    Ok(Runtime {
        state: MonitorState::Active,
        generation: 1,
        created_at_ms: now_ms,
        lifetime_deadline_ms,
        next_check_ms,
        next_notification_ms,
        check_count: 0,
        notification_count: 0,
        last_event_id: 0,
        last_event_reason: None,
        condition_matched: false,
        matcher_states: [0; pattern_word_count],
        path_baseline: None,
        probe_cwd_fingerprint: None,
    })
}

pub fn stable_id(buffer: &mut [u8], sequence: u64) -> Result<&[u8], MonitorError> {
    if sequence == 0 {
        return Err(MonitorError::MonitorIdExhausted);
    }
    let text = format!("monitor-{sequence}");
    let bytes = text.as_bytes();
    if bytes.len() > buffer.len() {
        return Err(MonitorError::MonitorIdExhausted);
    }
    buffer[..bytes.len()].copy_from_slice(bytes);
    Ok(&buffer[..bytes.len()])
}

pub fn observe(
    monitor: &mut PersistedMonitor,
    observation: Observation,
    condition_matches: bool,
    now_ms: i64,
) -> Result<Decision, MonitorError> {
    if monitor.runtime.state == MonitorState::Paused
        || monitor.runtime.state == MonitorState::Degraded
    {
        return Ok(Decision::default());
    }
    if expired(&monitor.runtime, now_ms)? {
        return Ok(Decision {
            notify: state_notification(&monitor.definition, EventReason::Expired),
            remove: true,
            state_changed: true,
        });
    }
    if observation == Observation::SessionExit {
        return Ok(Decision {
            notify: if monitor.definition.notify_schedule == NotifySchedule::OnExit {
                Some(EventReason::SessionExit)
            } else {
                state_notification(&monitor.definition, EventReason::SessionExit)
            },
            remove: true,
            state_changed: true,
        });
    }
    monitor.runtime.check_count =
        monitor.runtime.check_count.checked_add(1).ok_or(MonitorError::CounterOverflow)?;
    if observation == Observation::Check {
        if let Some(schedule) = monitor.definition.check_schedule {
            monitor.runtime.next_check_ms = Some(advance_deadline(
                monitor.runtime.next_check_ms.unwrap_or(now_ms),
                schedule.interval_ms,
                now_ms,
            )?);
        }
    }
    if observation == Observation::Quiet {
        if let MonitorCondition::OutputQuietMs(quiet_ms) = monitor.definition.condition {
            monitor.runtime.next_check_ms = Some(deadline(now_ms, quiet_ms)?);
        }
    }

    let newly_matched = condition_matches && !monitor.runtime.condition_matched;
    if newly_matched {
        monitor.runtime.condition_matched = true;
        monitor.runtime.state = MonitorState::Matched;
    }

    let mut decision = Decision { state_changed: newly_matched, ..Decision::default() };
    decision.notify = match monitor.definition.notify_schedule {
        NotifySchedule::OnMatch => {
            if newly_matched { Some(EventReason::Matched) } else { None }
        }
        NotifySchedule::OnStateChange => {
            if newly_matched { Some(EventReason::StateChanged) } else { None }
        }
        NotifySchedule::OnExit | NotifySchedule::Interval(_) => None,
        NotifySchedule::EveryCheck => Some(EventReason::Check),
        NotifySchedule::EveryNChecks(count) => {
            if monitor.runtime.check_count % count as u64 == 0 {
                Some(EventReason::Check)
            } else {
                None
            }
        }
    };
    if newly_matched && monitor.definition.lifetime == MonitorLifetime::UntilMatch {
        decision.remove = true;
    }
    Ok(decision)
}

pub fn timer_decision(monitor: &mut PersistedMonitor, now_ms: i64) -> Result<Decision, MonitorError> {
    if expired(&monitor.runtime, now_ms)? {
        return Ok(Decision {
            notify: state_notification(&monitor.definition, EventReason::Expired),
            remove: true,
            state_changed: true,
        });
    }
    if monitor.runtime.state == MonitorState::Paused
        || monitor.runtime.state == MonitorState::Degraded
    {
        return Ok(Decision::default());
    }
    let schedule = match monitor.definition.notify_schedule {
        NotifySchedule::Interval(value) => value,
        _ => return Ok(Decision::default()),
    };
    let due = monitor
        .runtime
        .next_notification_ms
        .ok_or(MonitorError::InvalidMonitorState)?;
    if now_ms < due {
        return Ok(Decision::default());
    }
    monitor.runtime.next_notification_ms = Some(advance_deadline(due, schedule.interval_ms, now_ms)?);
    Ok(Decision { notify: Some(EventReason::Interval), ..Decision::default() })
}

pub fn quiet_output(monitor: &mut PersistedMonitor, now_ms: i64) -> Result<(), MonitorError> {
    let quiet_ms = match monitor.definition.condition {
        MonitorCondition::OutputQuietMs(value) => value,
        _ => return Ok(()),
    };
    monitor.runtime.next_check_ms = Some(deadline(now_ms, quiet_ms)?);
    Ok(())
}

pub fn quiet_due(monitor: &PersistedMonitor, now_ms: i64) -> bool {
    if monitor.runtime.state == MonitorState::Paused {
        return false;
    }
    if !matches!(monitor.definition.condition, MonitorCondition::OutputQuietMs(_)) {
        return false;
    }
    monitor.runtime.next_check_ms.is_some_and(|due| now_ms >= due)
}

pub fn polling_due(monitor: &PersistedMonitor, now_ms: i64) -> bool {
    if monitor.runtime.state == MonitorState::Paused
        || monitor.runtime.state == MonitorState::Degraded
        || !monitor.definition.condition.requires_polling()
    {
        return false;
    }
    monitor.runtime.next_check_ms.is_some_and(|due| now_ms >= due)
}

pub fn next_deadline(monitor: &PersistedMonitor) -> Option<i64> {
    if monitor.runtime.state == MonitorState::Paused
        || monitor.runtime.state == MonitorState::Degraded
    {
        return monitor.runtime.lifetime_deadline_ms;
    }
    let mut result = monitor.runtime.lifetime_deadline_ms;
    result = earlier(result, monitor.runtime.next_check_ms);
    result = earlier(result, monitor.runtime.next_notification_ms);
    result
}

pub fn pause(monitor: &mut PersistedMonitor) -> bool {
    if monitor.runtime.state == MonitorState::Paused
        || monitor.runtime.state == MonitorState::Degraded
    {
        return false;
    }
    monitor.runtime.state = MonitorState::Paused;
    true
}

pub fn degrade_for_raw_gap(monitor: &mut PersistedMonitor) -> Result<bool, MonitorError> {
    let affected = matches!(
        monitor.definition.condition,
        MonitorCondition::OutputContains(_)
            | MonitorCondition::OutputMatches(_)
            | MonitorCondition::OutputQuietMs(_)
            | MonitorCondition::ScreenMatches(_)
    );
    if !affected || monitor.runtime.state == MonitorState::Degraded {
        return Ok(false);
    }
    monitor.runtime.state = MonitorState::Degraded;
    monitor.runtime.matcher_states = [0; pattern_word_count];
    monitor.runtime.next_check_ms = None;
    monitor.runtime.next_notification_ms = None;
    bump_generation(monitor)?;
    Ok(true)
}

pub fn resume_monitor(monitor: &mut PersistedMonitor, now_ms: i64) -> Result<bool, MonitorError> {
    if monitor.runtime.state != MonitorState::Paused {
        return Ok(false);
    }
    monitor.runtime.state = if monitor.runtime.condition_matched {
        MonitorState::Matched
    } else {
        MonitorState::Active
    };
    if let Some(schedule) = monitor.definition.check_schedule {
        monitor.runtime.next_check_ms = Some(deadline(now_ms, schedule.interval_ms)?);
    }
    if let MonitorCondition::OutputQuietMs(quiet_ms) = monitor.definition.condition {
        monitor.runtime.next_check_ms = Some(deadline(now_ms, quiet_ms)?);
    }
    if let NotifySchedule::Interval(schedule) = monitor.definition.notify_schedule {
        monitor.runtime.next_notification_ms = Some(deadline(now_ms, schedule.interval_ms)?);
    }
    Ok(true)
}

pub fn bump_generation(monitor: &mut PersistedMonitor) -> Result<(), MonitorError> {
    monitor.runtime.generation =
        monitor.runtime.generation.checked_add(1).ok_or(MonitorError::CounterOverflow)?;
    Ok(())
}

pub fn note_notification(
    monitor: &mut PersistedMonitor,
    event_id: u64,
    reason: EventReason,
) -> Result<(), MonitorError> {
    if event_id == 0 || event_id <= monitor.runtime.last_event_id {
        return Err(MonitorError::InvalidEventId);
    }
    monitor.runtime.last_event_id = event_id;
    monitor.runtime.last_event_reason = Some(reason);
    monitor.runtime.notification_count =
        monitor.runtime.notification_count.checked_add(1).ok_or(MonitorError::CounterOverflow)?;
    Ok(())
}

pub fn validate_runtime(monitor: &PersistedMonitor) -> Result<(), MonitorError> {
    validate_definition(&monitor.definition)?;
    if monitor.monitor_id.is_empty()
        || monitor.monitor_id.len() > contracts::max_monitor_id_bytes
        || monitor.runtime.generation == 0
        || monitor.runtime.created_at_ms < 0
        || (monitor.runtime.last_event_id > 0 && monitor.runtime.notification_count == 0)
        || (monitor.runtime.last_event_id == 0) != monitor.runtime.last_event_reason.is_none()
    {
        return Err(MonitorError::InvalidMonitorState);
    }
    if let Some(value) = monitor.runtime.lifetime_deadline_ms {
        if value <= monitor.runtime.created_at_ms {
            return Err(MonitorError::InvalidMonitorState);
        }
    }
    if monitor.runtime.condition_matched && monitor.runtime.state == MonitorState::Active {
        return Err(MonitorError::InvalidMonitorState);
    }
    Ok(())
}

pub fn pattern_feed(
    pattern: &[u8],
    wildcard: bool,
    states: &mut [u64; pattern_word_count],
    bytes: &[u8],
) -> Result<bool, MonitorError> {
    if pattern.is_empty() || pattern.len() > maximum_pattern_bytes {
        return Err(MonitorError::InvalidPattern);
    }
    set_bit(states, 0);
    epsilon_closure(pattern, wildcard, states);
    for &byte in bytes {
        let mut next = [0u64; pattern_word_count];
        set_bit(&mut next, 0);
        for index in 0..pattern.len() {
            if !bit_is_set(states, index) {
                continue;
            }
            let token = pattern[index];
            if wildcard && token == b'*' {
                set_bit(&mut next, index);
            } else if token == byte || (wildcard && token == b'?') {
                set_bit(&mut next, index + 1);
            }
        }
        epsilon_closure(pattern, wildcard, &mut next);
        *states = next;
        if bit_is_set(states, pattern.len()) {
            return Ok(true);
        }
    }
    Ok(bit_is_set(states, pattern.len()))
}

pub fn pattern_matches(
    pattern: &[u8],
    wildcard: bool,
    bytes: &[u8],
) -> Result<bool, MonitorError> {
    let mut states = [0u64; pattern_word_count];
    pattern_feed(pattern, wildcard, &mut states, bytes)
}

fn expired(runtime: &Runtime, now_ms: i64) -> Result<bool, MonitorError> {
    if now_ms < 0 {
        return Err(MonitorError::InvalidClock);
    }
    Ok(runtime.lifetime_deadline_ms.is_some_and(|value| now_ms >= value))
}

fn state_notification(
    definition: &MonitorDefinition,
    reason: EventReason,
) -> Option<EventReason> {
    if definition.notify_schedule == NotifySchedule::OnStateChange {
        Some(reason)
    } else {
        None
    }
}

fn advance_deadline(current: i64, interval_ms: u64, now_ms: i64) -> Result<i64, MonitorError> {
    let interval = i64::try_from(interval_ms).map_err(|_| MonitorError::DeadlineOverflow)?;
    if interval <= 0 {
        return Err(MonitorError::InvalidSchedule);
    }
    if current > now_ms {
        return Ok(current);
    }
    let elapsed = now_ms.checked_sub(current).ok_or(MonitorError::DeadlineOverflow)?;
    let steps = elapsed.div_euclid(interval) + 1;
    let offset = steps.checked_mul(interval).ok_or(MonitorError::DeadlineOverflow)?;
    current.checked_add(offset).ok_or(MonitorError::DeadlineOverflow)
}

fn earlier(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (None, _) => right,
        (_, None) => left,
        (Some(l), Some(r)) => Some(l.min(r)),
    }
}

fn epsilon_closure(pattern: &[u8], wildcard: bool, states: &mut [u64; pattern_word_count]) {
    if !wildcard {
        return;
    }
    for index in 0..pattern.len() {
        if pattern[index] == b'*' && bit_is_set(states, index) {
            set_bit(states, index + 1);
        }
    }
}

fn set_bit(states: &mut [u64; pattern_word_count], index: usize) {
    states[index / 64] |= 1u64 << (index % 64);
}

fn bit_is_set(states: &[u64; pattern_word_count], index: usize) -> bool {
    states[index / 64] & (1u64 << (index % 64)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::contracts::{CustomProbeCondition, Signal};

    fn test_definition(
        condition: MonitorCondition,
        notify_schedule: NotifySchedule,
        lifetime: MonitorLifetime,
    ) -> MonitorDefinition {
        let check_schedule = if condition.requires_polling() {
            Some(PollSchedule { interval_ms: 25 })
        } else {
            None
        };
        MonitorDefinition { condition, check_schedule, notify_schedule, lifetime }
    }

    #[test]
    fn bounded_matcher_preserves_literal_and_wildcard_state_across_chunks() {
        let mut literal = [0u64; pattern_word_count];
        assert!(!pattern_feed(b"ready", false, &mut literal, b"re").unwrap());
        assert!(pattern_feed(b"ready", false, &mut literal, b"ady").unwrap());

        let mut wildcard = [0u64; pattern_word_count];
        assert!(!pattern_feed(b"sta*t?d", true, &mut wildcard, b"xxsta").unwrap());
        assert!(!pattern_feed(b"sta*t?d", true, &mut wildcard, b"ble-t").unwrap());
        assert!(pattern_feed(b"sta*t?d", true, &mut wildcard, b"edyy").unwrap());
        assert!(pattern_matches(b"*status?ok*", true, b"prefix status-ok suffix").unwrap());
    }

    #[test]
    fn schedule_validation_and_deadline_arithmetic_reject_floods_and_overflow() {
        assert_eq!(validate_schedule(PollSchedule { interval_ms: 1 }), Err(MonitorError::InvalidSchedule));
        assert_eq!(
            validate_schedule(PollSchedule { interval_ms: maximum_schedule_ms + 1 }),
            Err(MonitorError::InvalidSchedule)
        );
        assert_eq!(
            validate_definition(&test_definition(
                MonitorCondition::OutputQuietMs(minimum_schedule_ms - 1),
                NotifySchedule::OnMatch,
                MonitorLifetime::UntilMatch,
            )),
            Err(MonitorError::InvalidMonitorCondition)
        );
        let long_pattern = vec![b'x'; maximum_pattern_bytes + 1];
        assert_eq!(
            validate_definition(&test_definition(
                MonitorCondition::OutputContains(long_pattern),
                NotifySchedule::OnMatch,
                MonitorLifetime::UntilMatch,
            )),
            Err(MonitorError::InvalidMonitorCondition)
        );
        assert_eq!(deadline(i64::MAX, 1), Err(MonitorError::DeadlineOverflow));
    }

    #[test]
    fn every_condition_follows_explicit_fake_clock_anchors_and_match_decisions() {
        let conditions = [
            MonitorCondition::ProcessExit,
            MonitorCondition::ExitCode(7),
            MonitorCondition::Signal(Signal::Terminate),
            MonitorCondition::OutputContains(b"ready".to_vec()),
            MonitorCondition::OutputMatches(b"re*dy".to_vec()),
            MonitorCondition::OutputQuietMs(25),
            MonitorCondition::ScreenMatches(b"ready".to_vec()),
            MonitorCondition::TcpReady { host: b"127.0.0.1".to_vec(), port: 3000 },
            MonitorCondition::HttpReady(b"http://127.0.0.1/health".to_vec()),
            MonitorCondition::PathExists(b"/workspace/ready".to_vec()),
            MonitorCondition::PathChanged(b"/workspace/output".to_vec()),
            MonitorCondition::PathSize { path: b"/workspace/output".to_vec(), minimum_bytes: 1 },
            MonitorCondition::CustomProbe(CustomProbeCondition {
                command: b"test -f ready".to_vec(),
                cwd: b"/workspace".to_vec(),
            }),
        ];
        for condition in conditions {
            let definition =
                test_definition(condition.clone(), NotifySchedule::OnMatch, MonitorLifetime::UntilMatch);
            let mut persisted = PersistedMonitor {
                monitor_id: b"monitor-1".to_vec(),
                definition: definition.clone(),
                runtime: initial_runtime(definition, 100).unwrap(),
            };
            let observation = if condition.requires_polling() {
                assert!(!polling_due(&persisted, 124));
                assert!(polling_due(&persisted, 125));
                Observation::Check
            } else if matches!(condition, MonitorCondition::OutputQuietMs(_)) {
                assert!(!quiet_due(&persisted, 124));
                assert!(quiet_due(&persisted, 125));
                Observation::Quiet
            } else if matches!(condition, MonitorCondition::ScreenMatches(_)) {
                Observation::Screen
            } else {
                Observation::Output
            };
            let decision = observe(&mut persisted, observation, true, 125).unwrap();
            assert_eq!(Some(EventReason::Matched), decision.notify);
            assert!(decision.remove);
            assert!(persisted.runtime.condition_matched);
            assert_eq!(MonitorState::Matched, persisted.runtime.state);
        }
    }

    #[test]
    fn every_check_schedules_count_each_real_evaluation_kind() {
        let cases = [
            (MonitorCondition::PathExists(b"/workspace/ready".to_vec()), Observation::Check),
            (MonitorCondition::OutputContains(b"ready".to_vec()), Observation::Output),
            (MonitorCondition::ScreenMatches(b"ready".to_vec()), Observation::Screen),
            (MonitorCondition::OutputQuietMs(25), Observation::Quiet),
            (MonitorCondition::ProcessExit, Observation::Exit),
        ];
        for (condition, observation) in cases {
            let every_definition = test_definition(
                condition.clone(),
                NotifySchedule::EveryCheck,
                MonitorLifetime::UntilSessionEnd,
            );
            let mut every = PersistedMonitor {
                monitor_id: b"monitor-1".to_vec(),
                definition: every_definition.clone(),
                runtime: initial_runtime(every_definition, 100).unwrap(),
            };
            assert_eq!(
                Some(EventReason::Check),
                observe(&mut every, observation, true, 110).unwrap().notify
            );
            assert_eq!(
                Some(EventReason::Check),
                observe(&mut every, observation, false, 120).unwrap().notify
            );
            assert_eq!(2, every.runtime.check_count);

            let every_two_definition = test_definition(
                condition.clone(),
                NotifySchedule::EveryNChecks(2),
                MonitorLifetime::DurationMs(100),
            );
            let mut every_two = PersistedMonitor {
                monitor_id: b"monitor-2".to_vec(),
                definition: every_two_definition.clone(),
                runtime: initial_runtime(every_two_definition, 100).unwrap(),
            };
            assert_eq!(None, observe(&mut every_two, observation, true, 110).unwrap().notify);
            assert_eq!(
                Some(EventReason::Check),
                observe(&mut every_two, observation, false, 120).unwrap().notify
            );
            assert_eq!(MonitorState::Matched, every_two.runtime.state);
            assert_eq!(
                None,
                observe(&mut every_two, Observation::SessionExit, false, 130).unwrap().notify
            );
        }
    }

    #[test]
    fn match_exit_and_interval_schedules_remain_independent_from_lifetime() {
        for schedule in [NotifySchedule::OnMatch, NotifySchedule::OnStateChange] {
            let definition =
                test_definition(MonitorCondition::ProcessExit, schedule, MonitorLifetime::UntilSessionEnd);
            let mut persisted = PersistedMonitor {
                monitor_id: b"monitor-1".to_vec(),
                definition: definition.clone(),
                runtime: initial_runtime(definition, 100).unwrap(),
            };
            assert!(observe(&mut persisted, Observation::Exit, true, 110).unwrap().notify.is_some());
        }
        let exit_definition =
            test_definition(MonitorCondition::ProcessExit, NotifySchedule::OnExit, MonitorLifetime::UntilSessionEnd);
        let mut on_exit = PersistedMonitor {
            monitor_id: b"monitor-2".to_vec(),
            definition: exit_definition.clone(),
            runtime: initial_runtime(exit_definition, 100).unwrap(),
        };
        assert_eq!(
            Some(EventReason::SessionExit),
            observe(&mut on_exit, Observation::SessionExit, false, 110).unwrap().notify
        );
        let interval_definition = test_definition(
            MonitorCondition::ProcessExit,
            NotifySchedule::Interval(PollSchedule { interval_ms: 25 }),
            MonitorLifetime::UntilSessionEnd,
        );
        let mut interval = PersistedMonitor {
            monitor_id: b"monitor-3".to_vec(),
            definition: interval_definition.clone(),
            runtime: initial_runtime(interval_definition, 100).unwrap(),
        };
        assert_eq!(
            Some(EventReason::Interval),
            timer_decision(&mut interval, 125).unwrap().notify
        );
    }

    #[test]
    fn quiet_deadline_resets_and_duration_expires_deterministically() {
        let definition = test_definition(
            MonitorCondition::OutputQuietMs(50),
            NotifySchedule::OnMatch,
            MonitorLifetime::DurationMs(100),
        );
        let mut persisted = PersistedMonitor {
            monitor_id: b"monitor-1".to_vec(),
            definition: definition.clone(),
            runtime: initial_runtime(definition, 1_000).unwrap(),
        };
        quiet_output(&mut persisted, 1_025).unwrap();
        assert!(!quiet_due(&persisted, 1_074));
        assert!(quiet_due(&persisted, 1_075));
        assert!(!observe(&mut persisted, Observation::Quiet, true, 1_075).unwrap().remove);
        assert!(timer_decision(&mut persisted, 1_100).unwrap().remove);
    }

    #[test]
    fn pause_resume_deduplication_and_stable_ids_are_deterministic() {
        let definition =
            test_definition(MonitorCondition::ProcessExit, NotifySchedule::OnMatch, MonitorLifetime::UntilSessionEnd);
        let mut persisted = PersistedMonitor {
            monitor_id: b"monitor-9".to_vec(),
            definition: definition.clone(),
            runtime: initial_runtime(definition, 10).unwrap(),
        };
        assert!(pause(&mut persisted));
        assert!(resume_monitor(&mut persisted, 20).unwrap());
        note_notification(&mut persisted, 4, EventReason::Matched).unwrap();
        assert_eq!(
            Err(MonitorError::InvalidEventId),
            note_notification(&mut persisted, 4, EventReason::Matched)
        );
        let mut buffer = [0u8; 64];
        assert_eq!(b"monitor-42", stable_id(&mut buffer, 42).unwrap());
    }

    #[test]
    fn duration_lifetime_expires_while_paused_without_retaining_a_past_deadline() {
        let definition = test_definition(
            MonitorCondition::ProcessExit,
            NotifySchedule::OnStateChange,
            MonitorLifetime::DurationMs(50),
        );
        let mut persisted = PersistedMonitor {
            monitor_id: b"monitor-1".to_vec(),
            definition: definition.clone(),
            runtime: initial_runtime(definition, 100).unwrap(),
        };
        assert!(pause(&mut persisted));
        assert_eq!(Some(150), next_deadline(&persisted));
        assert!(!timer_decision(&mut persisted, 149).unwrap().remove);
        let expired_decision = timer_decision(&mut persisted, 150).unwrap();
        assert!(expired_decision.remove);
        assert_eq!(Some(EventReason::Expired), expired_decision.notify);
    }

    #[test]
    fn raw_gaps_degrade_byte_and_screen_monitors_without_affecting_process_monitors() {
        for condition in [
            MonitorCondition::OutputContains(b"needle".to_vec()),
            MonitorCondition::OutputMatches(b"needle".to_vec()),
            MonitorCondition::OutputQuietMs(50),
            MonitorCondition::ScreenMatches(b"needle".to_vec()),
        ] {
            let definition =
                test_definition(condition, NotifySchedule::OnMatch, MonitorLifetime::UntilSessionEnd);
            let mut persisted = PersistedMonitor {
                monitor_id: b"monitor-gap".to_vec(),
                definition: definition.clone(),
                runtime: initial_runtime(definition, 100).unwrap(),
            };
            assert!(degrade_for_raw_gap(&mut persisted).unwrap());
            assert_eq!(MonitorState::Degraded, persisted.runtime.state);
            assert!(!observe(&mut persisted, Observation::Output, true, 110).unwrap().state_changed);
            assert!(!polling_due(&persisted, 110));
        }

        let definition =
            test_definition(MonitorCondition::ProcessExit, NotifySchedule::OnMatch, MonitorLifetime::UntilSessionEnd);
        let mut process = PersistedMonitor {
            monitor_id: b"monitor-process".to_vec(),
            definition: definition.clone(),
            runtime: initial_runtime(definition, 100).unwrap(),
        };
        assert!(!degrade_for_raw_gap(&mut process).unwrap());
        assert_eq!(MonitorState::Active, process.runtime.state);
    }
}
