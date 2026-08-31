//! Confirmed, generation-checked session permission state.
//!
//! Ported from `zig-reference/src/core/permissions/session_permission_state.zig`.

pub const SCHEMA_VERSION: u8 = 2;
pub const MAX_RULES: usize = 1024;
pub const MAX_IDENTITY_BYTES: usize = 4096;
pub const MAX_DISPLAY_IDENTITY_BYTES: usize = 4096;
#[allow(non_upper_case_globals)]
pub const schema_version: u8 = SCHEMA_VERSION;
#[allow(non_upper_case_globals)]
pub const max_rules: usize = MAX_RULES;
#[allow(non_upper_case_globals)]
pub const max_identity_bytes: usize = MAX_IDENTITY_BYTES;
#[allow(non_upper_case_globals)]
pub const max_display_identity_bytes: usize = MAX_DISPLAY_IDENTITY_BYTES;
const LEGACY_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuleId {
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleKeyKind {
    Command,
    FileMutation,
    StructuredTool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleKey {
    pub kind: RuleKeyKind,
    pub digest: [u8; 32],
    pub canonical: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidPermissionIdentity;

impl RuleKey {
    pub fn init(kind: RuleKeyKind, canonical: &[u8]) -> Result<Self, InvalidPermissionIdentity> {
        if canonical.is_empty() || canonical.len() > MAX_IDENTITY_BYTES {
            return Err(InvalidPermissionIdentity);
        }
        Ok(Self {
            kind,
            digest: sha256(canonical),
            canonical: canonical.to_vec(),
        })
    }
    pub fn eql(a: &Self, b: &Self) -> bool {
        a == b
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateDecision {
    Allow,
    Deny,
    Unresolved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rule {
    pub id: RuleId,
    pub key: RuleKey,
    pub display_identity: Vec<u8>,
    pub decision: Decision,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct State {
    pub version: u8,
    pub next_generation: u64,
    pub rules: Vec<Rule>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            next_generation: 1,
            rules: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidPermissionState;
pub fn validate(state: &State) -> Result<(), InvalidPermissionState> {
    validate_schema(state, SCHEMA_VERSION)
}
pub fn validate_schema(state: &State, expected_version: u8) -> Result<(), InvalidPermissionState> {
    if state.version != expected_version
        || state.next_generation == 0
        || state.rules.len() > MAX_RULES
    {
        return Err(InvalidPermissionState);
    }
    for (index, rule) in state.rules.iter().enumerate() {
        if rule.id.value == 0
            || rule.generation == 0
            || rule.id.value > rule.generation
            || rule.id.value >= state.next_generation
            || rule.generation >= state.next_generation
            || rule.key.canonical.is_empty()
            || rule.key.canonical.len() > MAX_IDENTITY_BYTES
            || rule.display_identity.is_empty()
            || rule.display_identity.len() > MAX_DISPLAY_IDENTITY_BYTES
            || sha256(&rule.key.canonical) != rule.key.digest
            || state.rules[..index]
                .iter()
                .any(|prior| prior.id == rule.id || prior.key == rule.key)
        {
            return Err(InvalidPermissionState);
        }
    }
    Ok(())
}

fn write_identity_field(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u64).to_be_bytes());
    out.extend_from_slice(value);
}
fn read_identity_field<'a>(
    canonical: &'a [u8],
    offset: &mut usize,
) -> Result<&'a [u8], InvalidPermissionIdentity> {
    let end = offset
        .checked_add(8)
        .filter(|end| *end <= canonical.len())
        .ok_or(InvalidPermissionIdentity)?;
    let length = u64::from_be_bytes(canonical[*offset..end].try_into().unwrap());
    let length: usize = length.try_into().map_err(|_| InvalidPermissionIdentity)?;
    let value_end = end
        .checked_add(length)
        .filter(|value_end| *value_end <= canonical.len())
        .ok_or(InvalidPermissionIdentity)?;
    *offset = value_end;
    Ok(&canonical[end..value_end])
}

pub fn command_key_v2(
    command: &[u8],
    cwd: &[u8],
    background: &[u8],
    target_os: &[u8],
) -> Result<RuleKey, InvalidPermissionIdentity> {
    let mut canonical = Vec::new();
    for field in [
        b"fx-permission-state-v2".as_slice(),
        command,
        cwd,
        background,
        target_os,
    ] {
        write_identity_field(&mut canonical, field);
    }
    RuleKey::init(RuleKeyKind::Command, &canonical)
}

struct V1CommandIdentity<'a> {
    command: &'a [u8],
    cwd: &'a [u8],
    background: &'a [u8],
    backend: &'a [u8],
    target_os: &'a [u8],
}
fn parse_v1_command_identity(
    canonical: &[u8],
) -> Result<V1CommandIdentity<'_>, InvalidPermissionIdentity> {
    let mut offset = 0;
    if read_identity_field(canonical, &mut offset)? != b"fx-permission-state-v1" {
        return Err(InvalidPermissionIdentity);
    }
    let command = read_identity_field(canonical, &mut offset)?;
    let cwd = read_identity_field(canonical, &mut offset)?;
    let background = read_identity_field(canonical, &mut offset)?;
    let backend = read_identity_field(canonical, &mut offset)?;
    let target_os = read_identity_field(canonical, &mut offset)?;
    let _provenance = read_identity_field(canonical, &mut offset)?;
    if offset != canonical.len()
        || command.is_empty()
        || cwd.is_empty()
        || target_os.is_empty()
        || !matches!(background, b"foreground" | b"background")
    {
        return Err(InvalidPermissionIdentity);
    }
    Ok(V1CommandIdentity {
        command,
        cwd,
        background,
        backend,
        target_os,
    })
}

pub fn migrate_v1_to_v2(state: &State) -> Result<State, InvalidPermissionState> {
    if state.version == SCHEMA_VERSION {
        validate_schema(state, SCHEMA_VERSION)?;
        return Ok(state.clone());
    }
    validate_schema(state, LEGACY_SCHEMA_VERSION)?;
    let mut migrated = State {
        version: SCHEMA_VERSION,
        next_generation: state.next_generation,
        rules: Vec::with_capacity(state.rules.len()),
    };
    for rule in &state.rules {
        if rule.key.kind != RuleKeyKind::Command {
            migrated.rules.push(rule.clone());
            continue;
        }
        let identity =
            parse_v1_command_identity(&rule.key.canonical).map_err(|_| InvalidPermissionState)?;
        if rule.decision == Decision::Allow && identity.backend != b"none" {
            continue;
        }
        let key = command_key_v2(
            identity.command,
            identity.cwd,
            identity.background,
            identity.target_os,
        )
        .map_err(|_| InvalidPermissionState)?;
        if let Some(index) = migrated.rules.iter().position(|current| current.key == key) {
            let current = &mut migrated.rules[index];
            if current.decision == Decision::Deny && rule.decision == Decision::Allow {
                continue;
            }
            if current.decision == Decision::Allow && rule.decision == Decision::Deny {
                current.display_identity = rule.display_identity.clone();
                current.id = rule.id;
                current.generation = rule.generation;
                current.decision = Decision::Deny;
                continue;
            }
            if rule.generation > current.generation
                || (rule.generation == current.generation
                    && rule.display_identity < current.display_identity)
            {
                current.display_identity = rule.display_identity.clone();
            }
            if rule.id.value < current.id.value {
                current.id = rule.id;
            }
            if rule.generation > current.generation {
                current.generation = rule.generation;
            }
        } else {
            let mut copied = rule.clone();
            copied.key = key;
            migrated.rules.push(copied);
        }
    }
    migrated.rules.sort_by_key(|rule| rule.id.value);
    validate_schema(&migrated, SCHEMA_VERSION)?;
    Ok(migrated)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetEvent {
    pub key: RuleKey,
    pub display_identity: Vec<u8>,
    pub decision: Decision,
    pub expected_generation: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RevokeEvent {
    pub id: RuleId,
    pub expected_generation: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfirmedRuleEvent {
    Set(SetEvent),
    Revoke(RevokeEvent),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedConfirmedRuleEvent {
    pub event: ConfirmedRuleEvent,
}
impl OwnedConfirmedRuleEvent {
    pub fn dupe(event: &ConfirmedRuleEvent) -> Self {
        Self {
            event: event.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyResult {
    Applied(State),
    Stale,
    Full,
    Invalid,
}
impl ApplyResult {
    pub fn take_applied(&mut self) -> Option<State> {
        if let Self::Applied(_) = self {
            if let Self::Applied(state) = std::mem::replace(self, Self::Invalid) {
                return Some(state);
            }
        }
        None
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyStatus {
    Applied,
    Stale,
    Full,
    Invalid,
}

pub fn apply(state: &State, event: &ConfirmedRuleEvent) -> ApplyResult {
    if validate(state).is_err() {
        return ApplyResult::Invalid;
    }
    match event {
        ConfirmedRuleEvent::Set(set) => apply_set(state, set),
        ConfirmedRuleEvent::Revoke(revoke) => apply_revoke(state, *revoke),
    }
}
fn apply_set(state: &State, event: &SetEvent) -> ApplyResult {
    if event.display_identity.is_empty()
        || event.display_identity.len() > MAX_DISPLAY_IDENTITY_BYTES
        || event.key.canonical.is_empty()
        || event.key.canonical.len() > MAX_IDENTITY_BYTES
    {
        return ApplyResult::Invalid;
    }
    let Some(next_generation) = state.next_generation.checked_add(1) else {
        return ApplyResult::Invalid;
    };
    if let Some(index) = state.rules.iter().position(|rule| rule.key == event.key) {
        if event.expected_generation != Some(state.rules[index].generation) {
            return ApplyResult::Stale;
        }
        let mut next = state.clone();
        let rule = &mut next.rules[index];
        rule.display_identity = event.display_identity.clone();
        rule.decision = event.decision;
        rule.generation = state.next_generation;
        next.next_generation = next_generation;
        return ApplyResult::Applied(next);
    }
    if event.expected_generation.is_some() {
        return ApplyResult::Stale;
    }
    if state.rules.len() >= MAX_RULES {
        return ApplyResult::Full;
    }
    let mut next = state.clone();
    next.rules.push(Rule {
        id: RuleId {
            value: state.next_generation,
        },
        key: event.key.clone(),
        display_identity: event.display_identity.clone(),
        decision: event.decision,
        generation: state.next_generation,
    });
    next.next_generation = next_generation;
    ApplyResult::Applied(next)
}
fn apply_revoke(state: &State, event: RevokeEvent) -> ApplyResult {
    let Some(index) = state.rules.iter().position(|rule| rule.id == event.id) else {
        return ApplyResult::Stale;
    };
    if state.rules[index].generation != event.expected_generation {
        return ApplyResult::Stale;
    }
    let Some(next_generation) = state.next_generation.checked_add(1) else {
        return ApplyResult::Invalid;
    };
    let mut next = state.clone();
    next.rules.remove(index);
    next.next_generation = next_generation;
    ApplyResult::Applied(next)
}

pub fn dupe(state: &State) -> Result<State, InvalidPermissionState> {
    validate(state)?;
    Ok(state.clone())
}
pub fn rule_for_id(state: &State, id: RuleId) -> Option<&Rule> {
    state.rules.iter().find(|rule| rule.id == id)
}
pub fn rule_for_key<'a>(state: &'a State, key: &RuleKey) -> Option<&'a Rule> {
    state.rules.iter().find(|rule| &rule.key == key)
}
pub fn decide(state: &State, key: &RuleKey) -> StateDecision {
    match rule_for_key(state, key).map(|rule| rule.decision) {
        Some(Decision::Allow) => StateDecision::Allow,
        Some(Decision::Deny) => StateDecision::Deny,
        None => StateDecision::Unresolved,
    }
}
pub fn project_for_child(state: &State, delegated_allow_keys: &[RuleKey]) -> State {
    State {
        version: state.version,
        next_generation: state.next_generation,
        rules: state
            .rules
            .iter()
            .filter(|rule| {
                rule.decision == Decision::Deny || delegated_allow_keys.contains(&rule.key)
            })
            .cloned()
            .collect(),
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let b = i * 4;
            *word = u32::from_be_bytes(chunk[b..b + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut z) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = z
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            z = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, z].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(kind: RuleKeyKind, value: &[u8]) -> RuleKey {
        RuleKey::init(kind, value).unwrap()
    }
    fn set(
        key: RuleKey,
        display: &[u8],
        decision: Decision,
        expected_generation: Option<u64>,
    ) -> ConfirmedRuleEvent {
        ConfirmedRuleEvent::Set(SetEvent {
            key,
            display_identity: display.to_vec(),
            decision,
            expected_generation,
        })
    }
    #[test]
    fn insertion_replacement_and_revocation_are_generation_checked() {
        let empty = State::default();
        let k = key(RuleKeyKind::Command, b"command\0git status");
        let ApplyResult::Applied(inserted) = apply(
            &empty,
            &set(k.clone(), b"git status", Decision::Allow, None),
        ) else {
            panic!()
        };
        let id = inserted.rules[0].id;
        let generation = inserted.rules[0].generation;
        assert_ne!(id.value, 0);
        assert_eq!(
            apply(
                &inserted,
                &set(
                    k.clone(),
                    b"replacement",
                    Decision::Deny,
                    Some(generation + 1)
                )
            ),
            ApplyResult::Stale
        );
        let ApplyResult::Applied(replaced) = apply(
            &inserted,
            &set(k.clone(), b"replacement", Decision::Deny, Some(generation)),
        ) else {
            panic!()
        };
        assert_eq!(replaced.rules[0].id, id);
        assert_eq!(inserted.rules[0].decision, Decision::Allow);
        assert_eq!(replaced.rules[0].decision, Decision::Deny);
        let ApplyResult::Applied(revoked) = apply(
            &replaced,
            &ConfirmedRuleEvent::Revoke(RevokeEvent {
                id,
                expected_generation: replaced.rules[0].generation,
            }),
        ) else {
            panic!()
        };
        assert!(revoked.rules.is_empty());
    }
    #[test]
    fn stale_events_and_generation_exhaustion_leave_state_unchanged() {
        let k = key(RuleKeyKind::Command, b"command\0git status");
        let ApplyResult::Applied(inserted) = apply(
            &State::default(),
            &set(k.clone(), b"git status", Decision::Deny, None),
        ) else {
            panic!()
        };
        let stored = inserted.rules[0].clone();
        assert_eq!(
            apply(
                &inserted,
                &set(
                    k,
                    b"replacement",
                    Decision::Allow,
                    Some(stored.generation + 1)
                )
            ),
            ApplyResult::Stale
        );
        assert_eq!(
            apply(
                &inserted,
                &ConfirmedRuleEvent::Revoke(RevokeEvent {
                    id: stored.id,
                    expected_generation: stored.generation + 1
                })
            ),
            ApplyResult::Stale
        );
        let exhausted = State {
            next_generation: u64::MAX,
            ..State::default()
        };
        assert_eq!(
            apply(
                &exhausted,
                &set(
                    key(RuleKeyKind::Command, b"new"),
                    b"new",
                    Decision::Allow,
                    None
                )
            ),
            ApplyResult::Invalid
        );
    }
    #[test]
    fn rule_revocation_uses_stored_id_after_identity_changes() {
        let original = key(RuleKeyKind::FileMutation, b"file\0old-preimage");
        let ApplyResult::Applied(inserted) = apply(
            &State::default(),
            &set(original, b"old", Decision::Deny, None),
        ) else {
            panic!()
        };
        let stored = inserted.rules[0].clone();
        assert_ne!(
            stored.key,
            key(RuleKeyKind::FileMutation, b"file\0new-preimage")
        );
        let ApplyResult::Applied(revoked) = apply(
            &inserted,
            &ConfirmedRuleEvent::Revoke(RevokeEvent {
                id: stored.id,
                expected_generation: stored.generation,
            }),
        ) else {
            panic!()
        };
        assert!(rule_for_id(&revoked, stored.id).is_none());
    }
    #[test]
    fn exact_decision_and_child_projection() {
        let deny = key(RuleKeyKind::Command, b"deny");
        let allow = key(RuleKeyKind::StructuredTool, b"allow");
        let ApplyResult::Applied(a) = apply(
            &State::default(),
            &set(deny.clone(), b"deny", Decision::Deny, None),
        ) else {
            panic!()
        };
        let ApplyResult::Applied(parent) =
            apply(&a, &set(allow.clone(), b"allow", Decision::Allow, None))
        else {
            panic!()
        };
        assert_eq!(
            decide(&parent, &key(RuleKeyKind::Command, b"neighbor")),
            StateDecision::Unresolved
        );
        let child = project_for_child(&parent, &[]);
        assert_eq!(decide(&child, &deny), StateDecision::Deny);
        assert_eq!(decide(&child, &allow), StateDecision::Unresolved);
        assert_eq!(
            decide(
                &project_for_child(&parent, std::slice::from_ref(&allow)),
                &allow
            ),
            StateDecision::Allow
        );
    }
    #[test]
    fn decision_matches_only_exact_canonical_key() {
        let exact = key(RuleKeyKind::Command, b"command\0/workspace\0git status");
        let ApplyResult::Applied(state) = apply(
            &State::default(),
            &set(exact.clone(), b"git status", Decision::Allow, None),
        ) else {
            panic!()
        };
        assert_eq!(decide(&state, &exact), StateDecision::Allow);
        assert_eq!(
            decide(
                &state,
                &key(
                    RuleKeyKind::Command,
                    b"command\0/workspace\0git status --short"
                )
            ),
            StateDecision::Unresolved
        );
    }
    #[test]
    fn child_projection_preserves_denies_and_filters_undelegated_allows() {
        let denied = key(RuleKeyKind::Command, b"denied");
        let allowed = key(RuleKeyKind::StructuredTool, b"allowed");
        let ApplyResult::Applied(first) = apply(
            &State::default(),
            &set(denied.clone(), b"denied", Decision::Deny, None),
        ) else {
            panic!()
        };
        let ApplyResult::Applied(parent) = apply(
            &first,
            &set(allowed.clone(), b"allowed", Decision::Allow, None),
        ) else {
            panic!()
        };
        let child = project_for_child(&parent, &[]);
        assert_eq!(decide(&child, &denied), StateDecision::Deny);
        assert_eq!(decide(&child, &allowed), StateDecision::Unresolved);
    }
    #[test]
    fn validation_rejects_duplicate_ids_and_capacity_preserves_replacement() {
        let mut state = State {
            next_generation: (MAX_RULES + 1) as u64,
            ..State::default()
        };
        for i in 0..MAX_RULES {
            state.rules.push(Rule {
                id: RuleId {
                    value: (i + 1) as u64,
                },
                key: key(RuleKeyKind::Command, format!("command-{i}").as_bytes()),
                display_identity: format!("command {i}").into_bytes(),
                decision: Decision::Deny,
                generation: (i + 1) as u64,
            });
        }
        validate(&state).unwrap();
        assert_eq!(
            apply(
                &state,
                &set(
                    key(RuleKeyKind::Command, b"new"),
                    b"new",
                    Decision::Allow,
                    None
                )
            ),
            ApplyResult::Full
        );
        let existing = state.rules[0].clone();
        assert!(matches!(
            apply(
                &state,
                &set(
                    existing.key,
                    b"replacement",
                    Decision::Allow,
                    Some(existing.generation)
                )
            ),
            ApplyResult::Applied(_)
        ));
        state.rules[1].id = state.rules[0].id;
        assert_eq!(validate(&state), Err(InvalidPermissionState));
    }
    fn identity(fields: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for field in fields {
            write_identity_field(&mut out, field);
        }
        out
    }
    #[test]
    fn v1_command_migration_is_deny_dominant_stable_and_idempotent() {
        let mut v1 = State {
            version: 1,
            next_generation: 5,
            rules: Vec::new(),
        };
        for (id, generation, command, backend, decision, display) in [
            (
                1,
                1,
                b"git status".as_slice(),
                b"none".as_slice(),
                Decision::Allow,
                b"none allow".as_slice(),
            ),
            (
                2,
                2,
                b"git status".as_slice(),
                b"macos".as_slice(),
                Decision::Deny,
                b"macos deny".as_slice(),
            ),
            (
                3,
                3,
                b"ps".as_slice(),
                b"macos".as_slice(),
                Decision::Allow,
                b"skip".as_slice(),
            ),
        ] {
            let canonical = identity(&[
                b"fx-permission-state-v1",
                command,
                b"/workspace",
                b"foreground",
                backend,
                b"macos",
                b"restricted",
            ]);
            v1.rules.push(Rule {
                id: RuleId { value: id },
                key: key(RuleKeyKind::Command, &canonical),
                display_identity: display.to_vec(),
                decision,
                generation,
            });
        }
        validate_schema(&v1, 1).unwrap();
        let migrated = migrate_v1_to_v2(&v1).unwrap();
        assert_eq!(migrated.version, 2);
        assert_eq!(migrated.rules.len(), 1);
        assert_eq!(migrated.rules[0].decision, Decision::Deny);
        assert_eq!(migrated.rules[0].display_identity, b"macos deny");
        assert_eq!(migrate_v1_to_v2(&migrated).unwrap(), migrated);
    }
}
