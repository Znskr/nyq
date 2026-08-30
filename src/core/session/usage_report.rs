//! Usage reporting and rolling snapshot projection.
//!
//! Mirrors `zig-reference/src/core/session/usage_report.zig`. All arithmetic
//! is checked so a corrupt ledger can never overflow a counter silently.

use crate::core::shared::{sort_utils, types};

const MAX_MODEL_BYTES: usize = 1024;
const MAX_MODELS: usize = 128;

const MS_PER_SECOND: i64 = 1000;
const MS_PER_MINUTE: i64 = 60 * MS_PER_SECOND;
const MS_PER_HOUR: i64 = 60 * MS_PER_MINUTE;
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    Session,
    Hours24,
    Days7,
    Days30,
}

impl Default for Scope {
    fn default() -> Self {
        Scope::Days30
    }
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Hours24 => "24 hours",
            Self::Days7 => "7 days",
            Self::Days30 => "30 days",
        }
    }

    pub fn cli_value(self) -> Option<&'static str> {
        match self {
            Self::Session => None,
            Self::Hours24 => Some("24h"),
            Self::Days7 => Some("7d"),
            Self::Days30 => Some("30d"),
        }
    }

    fn duration_ms(self) -> Option<i64> {
        match self {
            Self::Session => None,
            Self::Hours24 => Some(MS_PER_HOUR * 24),
            Self::Days7 => Some(MS_PER_DAY * 7),
            Self::Days30 => Some(MS_PER_DAY * 30),
        }
    }

    pub fn previous(self) -> Scope {
        match self {
            Self::Session => Self::Session,
            Self::Hours24 => Self::Session,
            Self::Days7 => Self::Hours24,
            Self::Days30 => Self::Days7,
        }
    }

    pub fn next(self) -> Scope {
        match self {
            Self::Session => Self::Hours24,
            Self::Hours24 => Self::Days7,
            Self::Days7 => Self::Days30,
            Self::Days30 => Self::Days30,
        }
    }
}

/// Formats a millisecond timestamp as a `Mon D, YYYY`-style date.
pub fn format_utc_date(timestamp_ms: i64) -> String {
    if timestamp_ms < 0 {
        return "Unknown".to_string();
    }
    let seconds = timestamp_ms.div_euclid(MS_PER_SECOND) as u64;
    let days = (seconds / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    let month_name = match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => return "Unknown".to_string(),
    };
    format!("{month_name} {day}, {year}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completeness {
    Complete,
    Pending,
    Incomplete,
    Legacy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Coverage {
    NotStarted,
    Partial,
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Incident {
    pub occurred_at_ms: i64,
    pub completeness: Completeness,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingMarker {
    pub id: Vec<u8>,
    pub observed_at_ms: i64,
}

impl PendingMarker {
    pub fn dupe(&self) -> PendingMarker {
        self.clone()
    }

    pub fn eql(&self, other: &PendingMarker) -> bool {
        self == other
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenerationFact {
    pub id: Vec<u8>,
    pub created_at_ms: i64,
    pub model: Vec<u8>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub billable_web_search_calls: u64,
    pub total_cost: f64,
}

impl GenerationFact {
    pub fn dupe(&self) -> GenerationFact {
        self.clone()
    }

    pub fn eql(&self, other: &GenerationFact) -> bool {
        self == other
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProfileEvent {
    Generation(GenerationFact),
    Pending(PendingMarker),
    Incident(Incident),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Totals {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub request_count: Option<u64>,
    pub total_cost: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelUsage {
    pub model: Vec<u8>,
    pub totals: Totals,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionActivity {
    pub api_duration_complete: bool,
    pub wall_duration_complete: bool,
    pub code_complete: bool,
    pub api_duration_ms: u64,
    pub wall_duration_ms: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionModelSource {
    pub model: Vec<u8>,
    pub total_cost: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub request_count: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSource {
    pub snapshot_time_ms: i64,
    pub session_started_at_ms: i64,
    pub completeness: Completeness,
    pub total_cost: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub request_count: Option<u64>,
    pub models: Vec<SessionModelSource>,
    pub activity: SessionActivity,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub scope: Scope,
    pub snapshot_time_ms: i64,
    pub window_start_ms: i64,
    pub coverage_started_at_ms: Option<i64>,
    pub coverage: Coverage,
    pub completeness: Completeness,
    pub totals: Option<Totals>,
    pub models: Vec<ModelUsage>,
    pub session_activity: Option<SessionActivity>,
}

impl Snapshot {
    pub fn deinit(&mut self) {
        self.models.clear();
    }

    pub fn dupe(&self) -> Snapshot {
        self.clone()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidGenerationFact;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidPendingMarker;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidUsageIncident;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSnapshotTime;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageCapacityExceeded;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageOverflow;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildError {
    InvalidGenerationFact,
    InvalidSnapshotTime,
    UsageCapacityExceeded,
    UsageOverflow,
}

#[derive(Clone, Default)]
struct MutableTotals {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: Option<u64>,
    request_count: u64,
    total_cost: f64,
}

impl MutableTotals {
    fn new() -> Self {
        Self {
            reasoning_tokens: Some(0),
            ..Self::default()
        }
    }

    fn add(&mut self, fact: &GenerationFact) -> Result<(), UsageOverflow> {
        self.input_tokens = self
            .input_tokens
            .checked_add(fact.input_tokens)
            .ok_or(UsageOverflow)?;
        self.output_tokens = self
            .output_tokens
            .checked_add(fact.output_tokens)
            .ok_or(UsageOverflow)?;
        self.cache_read_tokens = self
            .cache_read_tokens
            .checked_add(fact.cache_read_tokens)
            .ok_or(UsageOverflow)?;
        self.cache_write_tokens = self
            .cache_write_tokens
            .checked_add(fact.cache_write_tokens)
            .ok_or(UsageOverflow)?;
        self.request_count = self.request_count.checked_add(1).ok_or(UsageOverflow)?;
        self.reasoning_tokens = match (self.reasoning_tokens, fact.reasoning_tokens) {
            (Some(current), Some(reasoning)) => Some(current.checked_add(reasoning).ok_or(UsageOverflow)?),
            _ => None,
        };
        let next_cost = self.total_cost + fact.total_cost;
        if !next_cost.is_finite() {
            return Err(UsageOverflow);
        }
        self.total_cost = next_cost;
        Ok(())
    }

    fn freeze(self, request_count_available: bool) -> Result<Totals, UsageOverflow> {
        Ok(Totals {
            total_tokens: self
                .input_tokens
                .checked_add(self.output_tokens)
                .ok_or(UsageOverflow)?,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: if self.request_count == 0 {
                None
            } else {
                self.reasoning_tokens
            },
            request_count: if request_count_available {
                Some(self.request_count)
            } else {
                None
            },
            total_cost: self.total_cost,
        })
    }
}

struct MutableModel {
    model: Vec<u8>,
    totals: MutableTotals,
}

/// Converts an authoritative session aggregate into the same owned projection
/// used by rolling usage.
pub fn build_session_snapshot(source: SessionSource) -> Result<Snapshot, BuildError> {
    if source.snapshot_time_ms < 0
        || source.session_started_at_ms < 0
        || source.session_started_at_ms > source.snapshot_time_ms
        || !source.total_cost.is_finite()
        || source.total_cost < 0.0
        || source.cache_read_tokens > source.input_tokens
        || source.cache_write_tokens > source.input_tokens
    {
        return Err(BuildError::InvalidSnapshotTime);
    }
    if source
        .reasoning_tokens
        .is_some_and(|reasoning| reasoning > source.output_tokens)
    {
        return Err(BuildError::InvalidGenerationFact);
    }

    if source.completeness == Completeness::Legacy {
        let mut snapshot = empty_snapshot(
            Scope::Session,
            source.snapshot_time_ms,
            source.session_started_at_ms,
            Some(source.session_started_at_ms),
            Coverage::Full,
            source.completeness,
        );
        snapshot.session_activity = Some(source.activity);
        return Ok(snapshot);
    }
    if source.models.len() > MAX_MODELS {
        return Err(BuildError::UsageCapacityExceeded);
    }

    let total_tokens = source
        .input_tokens
        .checked_add(source.output_tokens)
        .ok_or(BuildError::UsageOverflow)?;

    let mut models = Vec::with_capacity(source.models.len());
    for source_model in &source.models {
        if source_model.model.is_empty()
            || source_model.model.len() > MAX_MODEL_BYTES
            || !source_model.total_cost.is_finite()
            || source_model.total_cost < 0.0
            || source_model.cache_read_tokens > source_model.input_tokens
            || source_model.cache_write_tokens > source_model.input_tokens
        {
            return Err(BuildError::InvalidGenerationFact);
        }
        if source_model
            .reasoning_tokens
            .is_some_and(|reasoning| reasoning > source_model.output_tokens)
        {
            return Err(BuildError::InvalidGenerationFact);
        }
        if !source_model
            .model
            .iter()
            .all(|&byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(BuildError::InvalidGenerationFact);
        }
        models.push(ModelUsage {
            model: source_model.model.clone(),
            totals: Totals {
                total_tokens: source_model
                    .input_tokens
                    .checked_add(source_model.output_tokens)
                    .ok_or(BuildError::UsageOverflow)?,
                input_tokens: source_model.input_tokens,
                output_tokens: source_model.output_tokens,
                cache_read_tokens: source_model.cache_read_tokens,
                cache_write_tokens: source_model.cache_write_tokens,
                reasoning_tokens: source_model.reasoning_tokens,
                request_count: source_model.request_count,
                total_cost: source_model.total_cost,
            },
        });
    }
    sort_utils::sort(&mut models, (), model_usage_less_than);

    Ok(Snapshot {
        scope: Scope::Session,
        snapshot_time_ms: source.snapshot_time_ms,
        window_start_ms: source.session_started_at_ms,
        coverage_started_at_ms: Some(source.session_started_at_ms),
        coverage: Coverage::Full,
        completeness: source.completeness,
        totals: Some(Totals {
            total_tokens,
            input_tokens: source.input_tokens,
            output_tokens: source.output_tokens,
            cache_read_tokens: source.cache_read_tokens,
            cache_write_tokens: source.cache_write_tokens,
            reasoning_tokens: source.reasoning_tokens,
            request_count: source.request_count,
            total_cost: source.total_cost,
        }),
        models,
        session_activity: Some(source.activity),
    })
}

/// Validates a generation fact.
pub fn validate_fact(fact: &GenerationFact) -> Result<(), InvalidGenerationFact> {
    if !types::valid_gateway_generation_id(&fact.id)
        || fact.created_at_ms < 0
        || fact.model.is_empty()
        || fact.model.len() > MAX_MODEL_BYTES
        || !fact.total_cost.is_finite()
        || fact.total_cost < 0.0
        || fact.cache_read_tokens > fact.input_tokens
        || fact.cache_write_tokens > fact.input_tokens
    {
        return Err(InvalidGenerationFact);
    }
    if fact
        .reasoning_tokens
        .is_some_and(|reasoning| reasoning > fact.output_tokens)
    {
        return Err(InvalidGenerationFact);
    }
    if !fact.model.iter().all(|&byte| (0x21..=0x7e).contains(&byte)) {
        return Err(InvalidGenerationFact);
    }
    Ok(())
}

/// Validates a pending marker.
pub fn validate_pending_marker(marker: &PendingMarker) -> Result<(), InvalidPendingMarker> {
    if !types::valid_gateway_generation_id(&marker.id) || marker.observed_at_ms < 0 {
        return Err(InvalidPendingMarker);
    }
    Ok(())
}

/// Validates a usage incident.
pub fn validate_incident(incident: Incident) -> Result<(), InvalidUsageIncident> {
    if incident.occurred_at_ms < 0
        || (incident.completeness != Completeness::Pending
            && incident.completeness != Completeness::Incomplete)
    {
        return Err(InvalidUsageIncident);
    }
    Ok(())
}

/// Builds an owned rolling snapshot from validated facts and an explicit time.
pub fn build_rolling_snapshot(
    scope: Scope,
    snapshot_time_ms: i64,
    coverage_started_at_ms: Option<i64>,
    facts: &[GenerationFact],
    incidents: &[Incident],
) -> Result<Snapshot, BuildError> {
    let duration_ms = scope.duration_ms().ok_or(BuildError::InvalidSnapshotTime)?;
    if snapshot_time_ms < 0 {
        return Err(BuildError::InvalidSnapshotTime);
    }
    let window_start_ms = snapshot_time_ms
        .checked_sub(duration_ms)
        .ok_or(BuildError::InvalidSnapshotTime)?;
    let mut visible_coverage_started_at_ms = coverage_started_at_ms;
    if let Some(started_at_ms) = coverage_started_at_ms {
        if started_at_ms < 0 {
            return Err(BuildError::InvalidSnapshotTime);
        }
        if started_at_ms > snapshot_time_ms {
            visible_coverage_started_at_ms = None;
        }
    }

    let mut completeness = Completeness::Complete;
    for incident in incidents {
        if incident.occurred_at_ms < window_start_ms
            || incident.occurred_at_ms >= snapshot_time_ms
        {
            continue;
        }
        completeness = match incident.completeness {
            Completeness::Incomplete | Completeness::Legacy => Completeness::Incomplete,
            Completeness::Pending => {
                if completeness == Completeness::Complete {
                    Completeness::Pending
                } else {
                    completeness
                }
            }
            Completeness::Complete => completeness,
        };
    }

    let coverage = match visible_coverage_started_at_ms {
        Some(started_at_ms) if started_at_ms <= window_start_ms => Coverage::Full,
        Some(_) => Coverage::Partial,
        None => Coverage::NotStarted,
    };
    if coverage == Coverage::NotStarted {
        return Ok(empty_snapshot(
            scope,
            snapshot_time_ms,
            window_start_ms,
            None,
            Coverage::NotStarted,
            completeness,
        ));
    }

    let mut seen: std::collections::HashMap<Vec<u8>, GenerationFact> =
        std::collections::HashMap::new();
    let mut model_indexes: std::collections::HashMap<Vec<u8>, usize> =
        std::collections::HashMap::new();
    let mut mutable_models: Vec<MutableModel> = Vec::new();
    let mut totals = MutableTotals::new();

    for fact in facts {
        validate_fact(fact).map_err(|_| BuildError::InvalidGenerationFact)?;
        if fact.created_at_ms < window_start_ms || fact.created_at_ms >= snapshot_time_ms {
            continue;
        }
        if let Some(existing) = seen.get(&fact.id) {
            if !existing.eql(fact) {
                completeness = Completeness::Incomplete;
            }
            continue;
        }
        seen.insert(fact.id.clone(), fact.clone());

        totals.add(fact).map_err(|_| BuildError::UsageOverflow)?;
        if !model_indexes.contains_key(&fact.model) {
            if mutable_models.len() == MAX_MODELS {
                return Err(BuildError::UsageCapacityExceeded);
            }
            mutable_models.push(MutableModel {
                model: fact.model.clone(),
                totals: MutableTotals::new(),
            });
            model_indexes.insert(fact.model.clone(), mutable_models.len() - 1);
        }
        let index = model_indexes[&fact.model];
        mutable_models[index]
            .totals
            .add(fact)
            .map_err(|_| BuildError::UsageOverflow)?;
    }

    let mut models = Vec::with_capacity(mutable_models.len());
    for model in mutable_models.into_iter() {
        models.push(ModelUsage {
            model: model.model,
            totals: model.totals.freeze(true).map_err(|_| BuildError::UsageOverflow)?,
        });
    }
    sort_utils::sort(&mut models, (), model_usage_less_than);

    Ok(Snapshot {
        scope,
        snapshot_time_ms,
        window_start_ms,
        coverage_started_at_ms: visible_coverage_started_at_ms,
        coverage,
        completeness,
        totals: Some(totals.freeze(true).map_err(|_| BuildError::UsageOverflow)?),
        models,
        session_activity: None,
    })
}

fn empty_snapshot(
    scope: Scope,
    snapshot_time_ms: i64,
    window_start_ms: i64,
    coverage_started_at_ms: Option<i64>,
    coverage: Coverage,
    completeness: Completeness,
) -> Snapshot {
    Snapshot {
        scope,
        snapshot_time_ms,
        window_start_ms,
        coverage_started_at_ms,
        coverage,
        completeness,
        totals: None,
        models: Vec::new(),
        session_activity: None,
    }
}

fn model_usage_less_than(_: &(), first: &ModelUsage, second: &ModelUsage) -> bool {
    if first.totals.total_tokens != second.totals.total_tokens {
        return first.totals.total_tokens > second.totals.total_tokens;
    }
    if first.totals.total_cost != second.totals.total_cost {
        return first.totals.total_cost > second.totals.total_cost;
    }
    first.model < second.model
}

#[cfg(test)]
fn test_fact(
    id: &str,
    model: &str,
    created_at_ms: i64,
    input_tokens: u64,
    output_tokens: u64,
    total_cost: f64,
) -> GenerationFact {
    GenerationFact {
        id: id.as_bytes().to_vec(),
        created_at_ms,
        model: model.as_bytes().to_vec(),
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: Some(0),
        billable_web_search_calls: 0,
        total_cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_snapshots_use_closed_open_boundaries_and_stable_usage_first_ordering() {
        let now = MS_PER_DAY * 40;
        let facts = [
            test_fact(
                "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "z/model",
                now - MS_PER_DAY * 30,
                3,
                2,
                1.0,
            ),
            test_fact(
                "gen_01ARZ3NDEKTSV4RRFFQ69G5FAW",
                "b/model",
                now - 1,
                5,
                5,
                2.0,
            ),
            test_fact(
                "gen_01ARZ3NDEKTSV4RRFFQ69G5FAX",
                "a/model",
                now - 2,
                5,
                5,
                2.0,
            ),
            test_fact(
                "gen_01ARZ3NDEKTSV4RRFFQ69G5FAY",
                "excluded/model",
                now,
                100,
                100,
                100.0,
            ),
        ];
        let snapshot = build_rolling_snapshot(Scope::Days30, now, Some(0), &facts, &[]).unwrap();

        assert_eq!(Coverage::Full, snapshot.coverage);
        assert_eq!(25, snapshot.totals.unwrap().total_tokens);
        assert_eq!(3, snapshot.models.len());
        assert_eq!(b"a/model", snapshot.models[0].model.as_slice());
        assert_eq!(b"b/model", snapshot.models[1].model.as_slice());
        assert_eq!(b"z/model", snapshot.models[2].model.as_slice());
    }

    #[test]
    fn rolling_snapshots_dedupe_exact_facts_and_fail_closed_on_conflicts_and_incidents() {
        let now = MS_PER_DAY * 40;
        let original = test_fact(
            "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "provider/model",
            now - 1,
            4,
            2,
            0.5,
        );
        let duplicate = original.clone();
        let mut conflict = original.clone();
        conflict.output_tokens = 3;

        let exact =
            build_rolling_snapshot(Scope::Hours24, now, Some(0), &[original.clone(), duplicate], &[])
                .unwrap();
        assert_eq!(6, exact.totals.unwrap().total_tokens);
        assert_eq!(1, exact.totals.unwrap().request_count.unwrap());

        let conflicted =
            build_rolling_snapshot(Scope::Hours24, now, Some(0), &[original.clone(), conflict], &[])
                .unwrap();
        assert_eq!(Completeness::Incomplete, conflicted.completeness);
        assert_eq!(6, conflicted.totals.unwrap().total_tokens);

        let pending = build_rolling_snapshot(
            Scope::Hours24,
            now,
            Some(0),
            &[original],
            &[Incident {
                occurred_at_ms: now - 1,
                completeness: Completeness::Pending,
            }],
        )
        .unwrap();
        assert_eq!(Completeness::Pending, pending.completeness);
        assert_eq!(6, pending.totals.unwrap().total_tokens);
    }

    #[test]
    fn rolling_snapshots_distinguish_tracking_start_from_measured_zero() {
        let now = MS_PER_DAY * 40;
        let unstarted = build_rolling_snapshot(Scope::Days7, now, None, &[], &[]).unwrap();
        assert_eq!(Coverage::NotStarted, unstarted.coverage);
        assert_eq!(Completeness::Complete, unstarted.completeness);
        assert!(unstarted.totals.is_none());

        let concurrently_started =
            build_rolling_snapshot(Scope::Days7, now, Some(now + 1), &[], &[]).unwrap();
        assert_eq!(Coverage::NotStarted, concurrently_started.coverage);
        assert_eq!(None, concurrently_started.coverage_started_at_ms);
        assert!(concurrently_started.totals.is_none());

        let unknown = build_rolling_snapshot(
            Scope::Days7,
            now,
            None,
            &[],
            &[Incident {
                occurred_at_ms: now - 1,
                completeness: Completeness::Incomplete,
            }],
        )
        .unwrap();
        assert_eq!(Coverage::NotStarted, unknown.coverage);
        assert_eq!(Completeness::Incomplete, unknown.completeness);
        assert!(unknown.totals.is_none());

        let measured =
            build_rolling_snapshot(Scope::Days7, now, Some(now - MS_PER_DAY), &[], &[]).unwrap();
        assert_eq!(Coverage::Partial, measured.coverage);
        assert_eq!(0, measured.totals.unwrap().total_tokens);
    }

    #[test]
    fn non_complete_session_snapshots_preserve_known_totals_and_activity() {
        let activity = SessionActivity {
            api_duration_complete: true,
            wall_duration_complete: false,
            code_complete: true,
            api_duration_ms: 1200,
            wall_duration_ms: 3400,
            lines_added: 5,
            lines_removed: 2,
        };
        for completeness in [Completeness::Pending, Completeness::Incomplete] {
            let snapshot = build_session_snapshot(SessionSource {
                snapshot_time_ms: 200,
                session_started_at_ms: 100,
                completeness,
                total_cost: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
                request_count: None,
                models: vec![],
                activity,
            })
            .unwrap();
            assert_eq!(0, snapshot.totals.unwrap().total_tokens);
            assert_eq!(activity, snapshot.session_activity.unwrap());
        }

        let legacy = build_session_snapshot(SessionSource {
            snapshot_time_ms: 200,
            session_started_at_ms: 100,
            completeness: Completeness::Legacy,
            total_cost: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            request_count: None,
            models: vec![],
            activity,
        })
        .unwrap();
        assert!(legacy.totals.is_none());
        assert_eq!(activity, legacy.session_activity.unwrap());
    }
}
