//! MCP feature-cache identity, freshness, and notification classification.
//!
//! Ported 1:1 from `zig-reference/src/core/mcp/feature_cache.zig`.
//! SHA-256 is a zero-dependency FIPS 180-4 implementation matching other ports.

pub type Digest = [u8; 32];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheScope {
    Private,
    Public,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    ToolsList,
    ResourcesList,
    ResourceTemplatesList,
    ResourceRead,
    PromptsList,
    PromptGet,
    Completion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheKey {
    pub server_identity: Digest,
    pub protocol_version: Digest,
    pub capability: Capability,
    pub normalized_arguments: Digest,
    pub private_auth_identity: Option<Digest>,
}

impl CacheKey {
    pub fn eql(left: CacheKey, right: CacheKey) -> bool {
        left.server_identity == right.server_identity
            && left.protocol_version == right.protocol_version
            && left.capability == right.capability
            && left.normalized_arguments == right.normalized_arguments
            && optional_digest_eql(left.private_auth_identity, right.private_auth_identity)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CacheKeyInput<'a> {
    pub server_identity: &'a [u8],
    pub protocol_version: &'a [u8],
    pub capability: Capability,
    pub normalized_arguments: &'a [u8],
    pub scope: CacheScope,
    pub auth_identity: Digest,
}

pub fn build_cache_key(input: CacheKeyInput<'_>) -> CacheKey {
    CacheKey {
        server_identity: digest(input.server_identity),
        protocol_version: digest(input.protocol_version),
        capability: input.capability,
        normalized_arguments: digest(input.normalized_arguments),
        private_auth_identity: if input.scope == CacheScope::Private {
            Some(input.auth_identity)
        } else {
            None
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8],
}

/// Produces an opaque partition identity without retaining header names or
/// credential values. Header order is significant because it is part of the
/// effective request identity.
pub fn auth_identity(headers: &[Header<'_>]) -> Digest {
    let mut hasher = Sha256::new();
    for header in headers {
        hash_length_prefixed(&mut hasher, header.name);
        hash_length_prefixed(&mut hasher, header.value);
    }
    hasher.finalize()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Freshness {
    Fresh,
    Stale,
    Refreshing,
    FailedRefresh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionIdentity {
    pub request_id: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotMetadata {
    pub key: CacheKey,
    pub connection_generation: u64,
    pub catalog_generation: u64,
    pub fetched_at_ms: u64,
    pub expires_at_ms: u64,
    pub scope: CacheScope,
    pub freshness: Freshness,
    pub refresh_attempt: u8,
    pub retry_at_ms: u64,
    pub content_digest: Digest,
    pub subscription: Option<SubscriptionIdentity>,
}

fn expiry_from_ttl(fetched_at_ms: u64, ttl_ms: u64) -> u64 {
    fetched_at_ms.saturating_add(ttl_ms)
}

pub fn page_expiry(protocol: Protocol, received_at_ms: u64, ttl_present: bool, ttl_ms: u64) -> u64 {
    expiry_from_ttl(received_at_ms, snapshot_ttl(protocol, ttl_present, ttl_ms))
}

pub fn earliest_expiry(current: Option<u64>, page_expiry_ms: u64) -> u64 {
    match current {
        Some(expiry_ms) => expiry_ms.min(page_expiry_ms),
        None => page_expiry_ms,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshAction {
    Hit,
    Refresh,
    RetryLater,
    AlreadyRefreshing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefreshDecision {
    pub action: RefreshAction,
    pub may_serve_snapshot: bool,
}

pub fn decide_refresh(
    metadata: SnapshotMetadata,
    requested_key: CacheKey,
    now_ms: u64,
    invalidated: bool,
) -> RefreshDecision {
    let same_partition = CacheKey::eql(metadata.key, requested_key);
    if !same_partition {
        return RefreshDecision {
            action: RefreshAction::Refresh,
            may_serve_snapshot: false,
        };
    }
    if metadata.freshness == Freshness::Refreshing {
        return RefreshDecision {
            action: RefreshAction::AlreadyRefreshing,
            may_serve_snapshot: true,
        };
    }
    if metadata.freshness == Freshness::FailedRefresh && now_ms < metadata.retry_at_ms {
        return RefreshDecision {
            action: RefreshAction::RetryLater,
            may_serve_snapshot: true,
        };
    }
    if invalidated {
        return RefreshDecision {
            action: RefreshAction::Refresh,
            may_serve_snapshot: true,
        };
    }
    if metadata.freshness == Freshness::Fresh && now_ms < metadata.expires_at_ms {
        return RefreshDecision {
            action: RefreshAction::Hit,
            may_serve_snapshot: true,
        };
    }
    RefreshDecision {
        action: RefreshAction::Refresh,
        may_serve_snapshot: true,
    }
}

pub fn effective_freshness(
    metadata: SnapshotMetadata,
    now_ms: u64,
    invalidated: bool,
) -> Freshness {
    match metadata.freshness {
        Freshness::Refreshing | Freshness::FailedRefresh => metadata.freshness,
        Freshness::Fresh | Freshness::Stale => {
            if invalidated || now_ms >= metadata.expires_at_ms {
                Freshness::Stale
            } else {
                Freshness::Fresh
            }
        }
    }
}

pub fn begin_refresh(metadata: SnapshotMetadata) -> SnapshotMetadata {
    let mut next = metadata;
    next.freshness = Freshness::Refreshing;
    next
}

const RETRY_INITIAL_MS: u64 = 100;
const RETRY_MAX_MS: u64 = 5_000;
const RETRY_MAX_ATTEMPT: u8 = 8;

pub fn retry_delay_ms(attempt: u8) -> u64 {
    let mut delay = RETRY_INITIAL_MS;
    let mut remaining = attempt.min(RETRY_MAX_ATTEMPT);
    while remaining > 0 {
        remaining -= 1;
        delay = delay.saturating_mul(2).min(RETRY_MAX_MS);
    }
    delay
}

pub fn failed_refresh(metadata: SnapshotMetadata, now_ms: u64) -> SnapshotMetadata {
    let mut next = metadata;
    next.freshness = Freshness::FailedRefresh;
    next.refresh_attempt = metadata.refresh_attempt.saturating_add(1).min(RETRY_MAX_ATTEMPT);
    next.retry_at_ms = now_ms.saturating_add(retry_delay_ms(metadata.refresh_attempt));
    next
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplacementDecision {
    RejectStaleGeneration,
    RejectCacheKey,
    MetadataOnly,
    ReplaceSnapshot,
}

pub fn authorize_replacement(
    current: SnapshotMetadata,
    source_connection_generation: u64,
    source_catalog_generation: u64,
    active_key: CacheKey,
    replacement_key: CacheKey,
    replacement_content_digest: Digest,
) -> ReplacementDecision {
    if current.connection_generation != source_connection_generation
        || current.catalog_generation != source_catalog_generation
    {
        return ReplacementDecision::RejectStaleGeneration;
    }
    if !CacheKey::eql(active_key, replacement_key) {
        return ReplacementDecision::RejectCacheKey;
    }
    if current.content_digest == replacement_content_digest {
        return ReplacementDecision::MetadataOnly;
    }
    ReplacementDecision::ReplaceSnapshot
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    Legacy,
    Modern,
}

fn snapshot_ttl(protocol: Protocol, ttl_present: bool, ttl_ms: u64) -> u64 {
    if ttl_present {
        return ttl_ms;
    }
    match protocol {
        Protocol::Modern => 0,
        Protocol::Legacy => u64::MAX,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationKind {
    SubscriptionAcknowledged,
    SubscriptionFilterUnsupported,
    ToolsListChanged,
    ResourcesListChanged,
    ResourceUpdated,
    PromptsListChanged,
    SubscriptionCancelled,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Notification<'a> {
    pub kind: NotificationKind,
    pub subscription_id: Option<u64>,
    pub cancelled_request_id: Option<u64>,
    pub resource_uri: Option<&'a [u8]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionExpectation {
    pub protocol: Protocol,
    pub capability_advertised: bool,
    pub connection_generation: u64,
    pub event_connection_generation: u64,
    pub subscription: Option<SubscriptionIdentity>,
    pub acknowledged: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationDecision {
    Acknowledge,
    Invalidate,
    CloseSubscription,
    CloseUnsupported,
    IgnoreUnadvertised,
    IgnoreMalformed,
    IgnoreDuplicate,
    IgnoreLateGeneration,
    IgnoreUnrelated,
}

pub fn classify_notification(
    expectation: SubscriptionExpectation,
    notification: Notification<'_>,
) -> NotificationDecision {
    if !expectation.capability_advertised {
        return NotificationDecision::IgnoreUnadvertised;
    }
    if expectation.connection_generation != expectation.event_connection_generation {
        return NotificationDecision::IgnoreLateGeneration;
    }
    if expectation.protocol == Protocol::Legacy {
        return match notification.kind {
            NotificationKind::ToolsListChanged
            | NotificationKind::ResourcesListChanged
            | NotificationKind::ResourceUpdated
            | NotificationKind::PromptsListChanged => NotificationDecision::Invalidate,
            _ => NotificationDecision::IgnoreUnrelated,
        };
    }

    let subscription = match expectation.subscription {
        Some(value) => value,
        None => return NotificationDecision::IgnoreLateGeneration,
    };
    match notification.kind {
        NotificationKind::SubscriptionAcknowledged => {
            let subscription_id = match notification.subscription_id {
                Some(id) => id,
                None => return NotificationDecision::IgnoreMalformed,
            };
            if subscription_id != subscription.request_id {
                return NotificationDecision::IgnoreLateGeneration;
            }
            if expectation.acknowledged {
                return NotificationDecision::IgnoreDuplicate;
            }
            NotificationDecision::Acknowledge
        }
        NotificationKind::SubscriptionFilterUnsupported => {
            let subscription_id = match notification.subscription_id {
                Some(id) => id,
                None => return NotificationDecision::IgnoreMalformed,
            };
            if subscription_id != subscription.request_id {
                return NotificationDecision::IgnoreLateGeneration;
            }
            if expectation.acknowledged {
                return NotificationDecision::IgnoreDuplicate;
            }
            NotificationDecision::CloseUnsupported
        }
        NotificationKind::ToolsListChanged
        | NotificationKind::ResourcesListChanged
        | NotificationKind::ResourceUpdated
        | NotificationKind::PromptsListChanged => {
            let subscription_id = match notification.subscription_id {
                Some(id) => id,
                None => return NotificationDecision::IgnoreMalformed,
            };
            if subscription_id != subscription.request_id {
                return NotificationDecision::IgnoreLateGeneration;
            }
            if !expectation.acknowledged {
                return NotificationDecision::IgnoreLateGeneration;
            }
            NotificationDecision::Invalidate
        }
        NotificationKind::SubscriptionCancelled => {
            let request_id = match notification.cancelled_request_id {
                Some(id) => id,
                None => return NotificationDecision::IgnoreMalformed,
            };
            if request_id == subscription.request_id {
                NotificationDecision::CloseSubscription
            } else {
                NotificationDecision::IgnoreLateGeneration
            }
        }
        NotificationKind::Other => NotificationDecision::IgnoreUnrelated,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoalesceDecision {
    Enqueue,
    AlreadyPending,
    Ignore,
}

pub fn decide_coalesce(pending: bool, notification: NotificationDecision) -> CoalesceDecision {
    if notification != NotificationDecision::Invalidate {
        return CoalesceDecision::Ignore;
    }
    if pending {
        CoalesceDecision::AlreadyPending
    } else {
        CoalesceDecision::Enqueue
    }
}

pub fn digest(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize()
}

fn optional_digest_eql(left: Option<Digest>, right: Option<Digest>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn hash_length_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

const SHA256_DIGEST_LENGTH: usize = 32;

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

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

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                0x1f83d9ab, 0x5be0cd19,
            ],
            buffer: [0; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buffer_len > 0 {
            let take = usize::min(64 - self.buffer_len, data.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; SHA256_DIGEST_LENGTH] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffer_len != 56 {
            self.update(&[0]);
        }
        self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut out = [0u8; SHA256_DIGEST_LENGTH];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

fn test_metadata(key: CacheKey, generation: u64, content: &[u8]) -> SnapshotMetadata {
    SnapshotMetadata {
        key,
        connection_generation: 4,
        catalog_generation: generation,
        fetched_at_ms: 100,
        expires_at_ms: 200,
        scope: if key.private_auth_identity.is_none() {
            CacheScope::Public
        } else {
            CacheScope::Private
        },
        freshness: Freshness::Fresh,
        refresh_attempt: 0,
        retry_at_ms: 0,
        content_digest: digest(content),
        subscription: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_notifications_are_idempotent() {
        let subscription = SubscriptionIdentity {
            request_id: 17,
            generation: 3,
        };
        let notification = Notification {
            kind: NotificationKind::ToolsListChanged,
            subscription_id: Some(17),
            cancelled_request_id: None,
            resource_uri: None,
        };
        let expectation = SubscriptionExpectation {
            protocol: Protocol::Modern,
            capability_advertised: true,
            connection_generation: 4,
            event_connection_generation: 4,
            subscription: Some(subscription),
            acknowledged: true,
        };
        let classified = classify_notification(expectation, notification);
        assert_eq!(NotificationDecision::Invalidate, classified);
        assert_eq!(
            CoalesceDecision::Enqueue,
            decide_coalesce(false, classified)
        );
        assert_eq!(
            CoalesceDecision::AlreadyPending,
            decide_coalesce(true, classified)
        );
    }

    #[test]
    fn failed_refresh_preserves_the_last_valid_snapshot() {
        let identity = auth_identity(&[]);
        let key = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Private,
            auth_identity: identity,
        });
        let current = test_metadata(key, 9, b"valid snapshot");
        let failed = failed_refresh(current, 250);
        assert_eq!(Freshness::FailedRefresh, failed.freshness);
        assert_eq!(current.catalog_generation, failed.catalog_generation);
        assert_eq!(current.content_digest, failed.content_digest);
        assert!(failed.retry_at_ms > 250);
    }

    #[test]
    fn a_stale_generation_cannot_replace_newer_state() {
        let key = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Public,
            auth_identity: auth_identity(&[]),
        });
        let current = test_metadata(key, 12, b"newer");
        assert_eq!(
            ReplacementDecision::RejectStaleGeneration,
            authorize_replacement(current, 4, 11, key, key, digest(b"older")),
        );
    }

    #[test]
    fn private_cache_state_cannot_cross_authentication_identities() {
        let first_identity = auth_identity(&[Header {
            name: b"Authorization",
            value: b"first",
        }]);
        let second_identity = auth_identity(&[Header {
            name: b"Authorization",
            value: b"second",
        }]);
        let first = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Private,
            auth_identity: first_identity,
        });
        let second = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Private,
            auth_identity: second_identity,
        });
        let decision = decide_refresh(test_metadata(first, 1, b"private"), second, 101, false);
        assert_eq!(RefreshAction::Refresh, decision.action);
        assert!(!decision.may_serve_snapshot);

        let public_first = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Public,
            auth_identity: first_identity,
        });
        let public_second = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Public,
            auth_identity: second_identity,
        });
        assert!(CacheKey::eql(public_first, public_second));
        assert_eq!(
            ReplacementDecision::RejectCacheKey,
            authorize_replacement(
                test_metadata(first, 1, b"private"),
                4,
                1,
                second,
                first,
                digest(b"replacement"),
            ),
        );
    }

    #[test]
    fn equivalent_validated_refreshes_do_not_replace_snapshot_content() {
        let key = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Public,
            auth_identity: auth_identity(&[]),
        });
        let current = test_metadata(key, 2, b"same schema");
        assert_eq!(
            ReplacementDecision::MetadataOnly,
            authorize_replacement(current, 4, 2, key, key, digest(b"same schema")),
        );
    }

    #[test]
    fn valid_changed_refresh_authorizes_one_whole_snapshot_replacement() {
        let key = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Public,
            auth_identity: auth_identity(&[]),
        });
        let current = test_metadata(key, 2, b"old complete snapshot");
        assert_eq!(
            ReplacementDecision::ReplaceSnapshot,
            authorize_replacement(current, 4, 2, key, key, digest(b"new complete snapshot")),
        );
    }

    #[test]
    fn expiry_and_retry_arithmetic_saturate() {
        assert_eq!(u64::MAX, expiry_from_ttl(u64::MAX - 1, 10));
        let mut metadata = test_metadata(
            build_cache_key(CacheKeyInput {
                server_identity: b"server",
                protocol_version: b"2026-07-28",
                capability: Capability::ToolsList,
                normalized_arguments: b"{}",
                scope: CacheScope::Public,
                auth_identity: auth_identity(&[]),
            }),
            1,
            b"snapshot",
        );
        metadata.refresh_attempt = RETRY_MAX_ATTEMPT;
        let failed = failed_refresh(metadata, u64::MAX - 1);
        assert_eq!(u64::MAX, failed.retry_at_ms);
        assert_eq!(RETRY_MAX_ATTEMPT, failed.refresh_attempt);
    }

    #[test]
    fn ttl_defaults_are_version_scoped_and_preserve_clock_boundaries() {
        assert_eq!(0, snapshot_ttl(Protocol::Modern, true, 0));
        assert_eq!(0, snapshot_ttl(Protocol::Modern, false, 99));
        assert_eq!(u64::MAX, snapshot_ttl(Protocol::Modern, true, u64::MAX));
        assert_eq!(u64::MAX, snapshot_ttl(Protocol::Legacy, false, 0));

        let key = build_cache_key(CacheKeyInput {
            server_identity: b"server",
            protocol_version: b"2026-07-28",
            capability: Capability::ToolsList,
            normalized_arguments: b"{}",
            scope: CacheScope::Public,
            auth_identity: auth_identity(&[]),
        });
        let mut metadata = test_metadata(key, 1, b"boundary");
        metadata.expires_at_ms = 500;
        assert_eq!(
            RefreshAction::Hit,
            decide_refresh(metadata, key, 499, false).action
        );
        assert_eq!(
            RefreshAction::Refresh,
            decide_refresh(metadata, key, 500, false).action
        );
    }

    #[test]
    fn paginated_cache_expiry_uses_each_page_receive_time() {
        let first_expiry = page_expiry(Protocol::Modern, 1_000, true, 100);
        assert_eq!(1_100, first_expiry);

        let later_expiry = page_expiry(Protocol::Modern, 1_050, true, 500);
        assert_eq!(first_expiry, earliest_expiry(Some(first_expiry), later_expiry));
        assert_eq!(
            u64::MAX,
            page_expiry(Protocol::Modern, u64::MAX - 1, true, 100)
        );
        assert_eq!(2_000, page_expiry(Protocol::Modern, 2_000, true, 0));
        assert_eq!(3_000, page_expiry(Protocol::Modern, 3_000, false, 99));
        assert_eq!(u64::MAX, page_expiry(Protocol::Legacy, 4_000, false, 0));
    }

    #[test]
    fn modern_notification_classification_rejects_malformed_late_and_duplicate_events() {
        let subscription = SubscriptionIdentity {
            request_id: 8,
            generation: 2,
        };
        let base = SubscriptionExpectation {
            protocol: Protocol::Modern,
            capability_advertised: true,
            connection_generation: 5,
            event_connection_generation: 5,
            subscription: Some(subscription),
            acknowledged: false,
        };
        assert_eq!(
            NotificationDecision::IgnoreMalformed,
            classify_notification(
                base,
                Notification {
                    kind: NotificationKind::SubscriptionAcknowledged,
                    subscription_id: None,
                    cancelled_request_id: None,
                    resource_uri: None,
                }
            ),
        );
        assert_eq!(
            NotificationDecision::IgnoreLateGeneration,
            classify_notification(
                base,
                Notification {
                    kind: NotificationKind::SubscriptionAcknowledged,
                    subscription_id: Some(9),
                    cancelled_request_id: None,
                    resource_uri: None,
                }
            ),
        );
        assert_eq!(
            NotificationDecision::Acknowledge,
            classify_notification(
                base,
                Notification {
                    kind: NotificationKind::SubscriptionAcknowledged,
                    subscription_id: Some(8),
                    cancelled_request_id: None,
                    resource_uri: None,
                }
            ),
        );
        let mut acknowledged = base;
        acknowledged.acknowledged = true;
        assert_eq!(
            NotificationDecision::IgnoreDuplicate,
            classify_notification(
                acknowledged,
                Notification {
                    kind: NotificationKind::SubscriptionAcknowledged,
                    subscription_id: Some(8),
                    cancelled_request_id: None,
                    resource_uri: None,
                }
            ),
        );
        let mut late = acknowledged;
        late.event_connection_generation = 4;
        assert_eq!(
            NotificationDecision::IgnoreLateGeneration,
            classify_notification(
                late,
                Notification {
                    kind: NotificationKind::ToolsListChanged,
                    subscription_id: Some(8),
                    cancelled_request_id: None,
                    resource_uri: None,
                }
            ),
        );
        assert_eq!(
            NotificationDecision::CloseSubscription,
            classify_notification(
                acknowledged,
                Notification {
                    kind: NotificationKind::SubscriptionCancelled,
                    subscription_id: None,
                    cancelled_request_id: Some(8),
                    resource_uri: None,
                }
            ),
        );
        assert_eq!(
            NotificationDecision::CloseUnsupported,
            classify_notification(
                base,
                Notification {
                    kind: NotificationKind::SubscriptionFilterUnsupported,
                    subscription_id: Some(8),
                    cancelled_request_id: None,
                    resource_uri: None,
                }
            ),
        );
        assert_eq!(
            NotificationDecision::IgnoreMalformed,
            classify_notification(
                base,
                Notification {
                    kind: NotificationKind::SubscriptionFilterUnsupported,
                    subscription_id: None,
                    cancelled_request_id: None,
                    resource_uri: None,
                }
            ),
        );
        assert_eq!(
            NotificationDecision::IgnoreLateGeneration,
            classify_notification(
                acknowledged,
                Notification {
                    kind: NotificationKind::SubscriptionCancelled,
                    subscription_id: None,
                    cancelled_request_id: Some(7),
                    resource_uri: None,
                }
            ),
        );
    }
}
