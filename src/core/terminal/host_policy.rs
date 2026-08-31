use super::contracts::CorrelationId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingError { DuplicateCorrelation, CapacityExceeded }

pub const queue_capacity: usize = 16;
pub const outcome_capacity: usize = 32;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IdleFacts {
    pub connected_clients: usize,
    pub pending_requests: usize,
    pub live_work: usize,
    pub monitor_required: bool,
}

pub fn idle_eligible(facts: IdleFacts) -> bool {
    facts.connected_clients == 0
        && facts.pending_requests == 0
        && facts.live_work == 0
        && !facts.monitor_required
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueAdmission { Admit, Full, Stopping }

pub fn classify_queue_admission(queued: usize, capacity: usize, stopping: bool) -> QueueAdmission {
    if stopping {
        return QueueAdmission::Stopping;
    }
    if queued >= capacity {
        return QueueAdmission::Full;
    }
    QueueAdmission::Admit
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionEvidence { Connected, EndpointMissing, Refused, Failed }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockEvidence { Acquired, Held, Unavailable }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityEvidence { Absent, Live, Dead, Unverifiable }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reconciliation {
    ReuseConnected,
    WaitForAuthority,
    StartHost,
    RemoveStaleThenStart,
    PreserveIdentityConflict,
    Unavailable,
}

pub fn classify_reconciliation(
    connection: ConnectionEvidence,
    lock: LockEvidence,
    identity: IdentityEvidence,
) -> Reconciliation {
    if connection == ConnectionEvidence::Connected {
        return Reconciliation::ReuseConnected;
    }
    match lock {
        LockEvidence::Held => Reconciliation::WaitForAuthority,
        LockEvidence::Unavailable => Reconciliation::Unavailable,
        LockEvidence::Acquired => match identity {
            IdentityEvidence::Live | IdentityEvidence::Unverifiable => {
                Reconciliation::PreserveIdentityConflict
            }
            IdentityEvidence::Absent | IdentityEvidence::Dead => {
                if connection == ConnectionEvidence::EndpointMissing {
                    Reconciliation::StartHost
                } else {
                    Reconciliation::RemoveStaleThenStart
                }
            }
        },
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PendingRequests {
    values: [Option<CorrelationId>; outcome_capacity],
}

impl Default for PendingRequests {
    fn default() -> Self {
        Self { values: [None; outcome_capacity] }
    }
}

impl PendingRequests {
    pub fn add(&mut self, correlation_id: CorrelationId) -> Result<(), PendingError> {
        if correlation_id.validate().is_err() {
            return Err(PendingError::DuplicateCorrelation);
        }
        let mut free_index: Option<usize> = None;
        for (index, entry) in self.values.iter().enumerate() {
            if let Some(existing) = entry {
                if existing.value == correlation_id.value {
                    return Err(PendingError::DuplicateCorrelation);
                }
            } else if free_index.is_none() {
                free_index = Some(index);
            }
        }
        match free_index {
            Some(index) => {
                self.values[index] = Some(correlation_id);
                Ok(())
            }
            None => Err(PendingError::CapacityExceeded),
        }
    }

    pub fn complete(&mut self, correlation_id: CorrelationId) -> bool {
        self.remove(correlation_id)
    }

    pub fn cancel(&mut self, correlation_id: CorrelationId) -> bool {
        self.remove(correlation_id)
    }

    pub fn contains(&self, correlation_id: CorrelationId) -> bool {
        self.values
            .iter()
            .any(|entry| entry.is_some_and(|existing| existing.value == correlation_id.value))
    }

    fn remove(&mut self, correlation_id: CorrelationId) -> bool {
        for entry in self.values.iter_mut() {
            let Some(existing) = *entry else { continue };
            if existing.value != correlation_id.value {
                continue;
            }
            *entry = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_eligibility_accounts_for_every_host_owner() {
        assert!(idle_eligible(IdleFacts::default()));
        assert!(!idle_eligible(IdleFacts { connected_clients: 1, ..Default::default() }));
        assert!(!idle_eligible(IdleFacts { pending_requests: 1, ..Default::default() }));
        assert!(!idle_eligible(IdleFacts { live_work: 1, ..Default::default() }));
        assert!(!idle_eligible(IdleFacts { monitor_required: true, ..Default::default() }));
    }

    #[test]
    fn queue_admission_is_bounded_and_rejects_shutdown() {
        assert_eq!(
            QueueAdmission::Admit,
            classify_queue_admission(queue_capacity - 1, queue_capacity, false),
        );
        assert_eq!(
            QueueAdmission::Full,
            classify_queue_admission(queue_capacity, queue_capacity, false),
        );
        assert_eq!(
            QueueAdmission::Stopping,
            classify_queue_admission(0, queue_capacity, true),
        );
    }

    #[test]
    fn reconciliation_requires_both_lock_and_liveness_proof_before_cleanup() {
        assert_eq!(
            Reconciliation::WaitForAuthority,
            classify_reconciliation(ConnectionEvidence::Refused, LockEvidence::Held, IdentityEvidence::Dead),
        );
        assert_eq!(
            Reconciliation::PreserveIdentityConflict,
            classify_reconciliation(ConnectionEvidence::Refused, LockEvidence::Acquired, IdentityEvidence::Live),
        );
        assert_eq!(
            Reconciliation::PreserveIdentityConflict,
            classify_reconciliation(ConnectionEvidence::Refused, LockEvidence::Acquired, IdentityEvidence::Unverifiable),
        );
        assert_eq!(
            Reconciliation::RemoveStaleThenStart,
            classify_reconciliation(ConnectionEvidence::Refused, LockEvidence::Acquired, IdentityEvidence::Dead),
        );
        assert_eq!(
            Reconciliation::StartHost,
            classify_reconciliation(ConnectionEvidence::EndpointMissing, LockEvidence::Acquired, IdentityEvidence::Absent),
        );
    }

    #[test]
    fn targeted_cancellation_releases_only_its_pending_request() {
        let mut pending = PendingRequests::default();
        let first = CorrelationId { value: 1 };
        let second = CorrelationId { value: 2 };
        assert!(pending.add(first).is_ok());
        assert!(pending.add(second).is_ok());
        assert!(pending.add(first).is_err());
        assert!(pending.cancel(first));
        assert!(!pending.contains(first));
        assert!(pending.contains(second));
        assert!(!pending.cancel(CorrelationId { value: 99 }));
        assert!(pending.complete(second));
    }
}
