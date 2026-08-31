use super::contracts::{Lifecycle, ScreenUnavailableReason};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordEvidence { Valid, Missing, Partial, Corrupt, Unsupported }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostEvidence { PresentSame, PresentForeign, Absent }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessEvidence { Matched, Missing, Mismatched, Unavailable }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointEvidence {
    ValidContiguous,
    Missing,
    Corrupt,
    Unsupported,
    Disconnected,
    RetentionEvicted,
    ResizeUncheckpointed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Input {
    pub record: RecordEvidence,
    pub lifecycle: Lifecycle,
    pub termination_present: bool,
    pub host: HostEvidence,
    pub process: ProcessEvidence,
    pub checkpoint: CheckpointEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    RetainLive,
    RetainFinal,
    FinalizeExited,
    MarkLost,
    IsolateCorrupt,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decision {
    pub disposition: Disposition,
    pub lifecycle: Lifecycle,
    pub screen_unavailable: Option<ScreenUnavailableReason>,
}

pub fn reconcile(input: Input) -> Decision {
    if input.record != RecordEvidence::Valid {
        return Decision {
            disposition: Disposition::IsolateCorrupt,
            lifecycle: Lifecycle::Lost,
            screen_unavailable: Some(record_screen_reason(input.record)),
        };
    }

    let screen_unavailable = checkpoint_reason(input.checkpoint);
    if input.lifecycle == Lifecycle::Exited
        || input.lifecycle == Lifecycle::Lost
        || input.lifecycle == Lifecycle::Closed
    {
        return Decision {
            disposition: Disposition::RetainFinal,
            lifecycle: input.lifecycle,
            screen_unavailable,
        };
    }
    if input.termination_present {
        return Decision {
            disposition: Disposition::FinalizeExited,
            lifecycle: Lifecycle::Exited,
            screen_unavailable,
        };
    }
    if input.host == HostEvidence::PresentSame && input.process == ProcessEvidence::Matched {
        return Decision {
            disposition: Disposition::RetainLive,
            lifecycle: input.lifecycle,
            screen_unavailable,
        };
    }
    if input.host == HostEvidence::PresentSame && input.process == ProcessEvidence::Unavailable {
        return Decision {
            disposition: Disposition::Unavailable,
            lifecycle: input.lifecycle,
            screen_unavailable,
        };
    }
    Decision {
        disposition: Disposition::MarkLost,
        lifecycle: Lifecycle::Lost,
        screen_unavailable,
    }
}

fn record_screen_reason(evidence: RecordEvidence) -> ScreenUnavailableReason {
    match evidence {
        RecordEvidence::Valid | RecordEvidence::Missing => ScreenUnavailableReason::Missing,
        RecordEvidence::Partial | RecordEvidence::Corrupt => ScreenUnavailableReason::Corrupt,
        RecordEvidence::Unsupported => ScreenUnavailableReason::UnsupportedSchema,
    }
}

fn checkpoint_reason(evidence: CheckpointEvidence) -> Option<ScreenUnavailableReason> {
    match evidence {
        CheckpointEvidence::ValidContiguous => None,
        CheckpointEvidence::Missing => Some(ScreenUnavailableReason::Missing),
        CheckpointEvidence::Corrupt => Some(ScreenUnavailableReason::Corrupt),
        CheckpointEvidence::Unsupported => Some(ScreenUnavailableReason::UnsupportedSchema),
        CheckpointEvidence::Disconnected => Some(ScreenUnavailableReason::RawGap),
        CheckpointEvidence::RetentionEvicted => Some(ScreenUnavailableReason::RetentionEvicted),
        CheckpointEvidence::ResizeUncheckpointed => {
            Some(ScreenUnavailableReason::ResizeUncheckpointed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_never_restarts_and_isolates_invalid_durable_records() {
        for record in [
            RecordEvidence::Missing,
            RecordEvidence::Partial,
            RecordEvidence::Corrupt,
            RecordEvidence::Unsupported,
        ] {
            let decision = reconcile(Input {
                record,
                lifecycle: Lifecycle::Running,
                termination_present: false,
                host: HostEvidence::PresentSame,
                process: ProcessEvidence::Matched,
                checkpoint: CheckpointEvidence::ValidContiguous,
            });
            assert_eq!(Disposition::IsolateCorrupt, decision.disposition);
        }
    }

    #[test]
    fn recovery_retains_only_a_matching_process_behind_the_same_host() {
        let retained = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Running,
            termination_present: false,
            host: HostEvidence::PresentSame,
            process: ProcessEvidence::Matched,
            checkpoint: CheckpointEvidence::ValidContiguous,
        });
        assert_eq!(Disposition::RetainLive, retained.disposition);

        let host_absent = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Running,
            termination_present: false,
            host: HostEvidence::Absent,
            process: ProcessEvidence::Matched,
            checkpoint: CheckpointEvidence::Missing,
        });
        assert_eq!(Disposition::MarkLost, host_absent.disposition);
        assert_eq!(Lifecycle::Lost, host_absent.lifecycle);

        let mismatched = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Starting,
            termination_present: false,
            host: HostEvidence::PresentSame,
            process: ProcessEvidence::Mismatched,
            checkpoint: CheckpointEvidence::Disconnected,
        });
        assert_eq!(Disposition::MarkLost, mismatched.disposition);
        assert_eq!(
            Some(ScreenUnavailableReason::RawGap),
            mismatched.screen_unavailable
        );
    }

    #[test]
    fn recovery_finalizes_durable_termination_and_preserves_completed_records() {
        let exited = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Running,
            termination_present: true,
            host: HostEvidence::Absent,
            process: ProcessEvidence::Missing,
            checkpoint: CheckpointEvidence::RetentionEvicted,
        });
        assert_eq!(Disposition::FinalizeExited, exited.disposition);
        assert_eq!(Lifecycle::Exited, exited.lifecycle);
        assert_eq!(
            Some(ScreenUnavailableReason::RetentionEvicted),
            exited.screen_unavailable
        );

        let closed = reconcile(Input {
            record: RecordEvidence::Valid,
            lifecycle: Lifecycle::Closed,
            termination_present: false,
            host: HostEvidence::Absent,
            process: ProcessEvidence::Missing,
            checkpoint: CheckpointEvidence::Corrupt,
        });
        assert_eq!(Disposition::RetainFinal, closed.disposition);
    }
}
