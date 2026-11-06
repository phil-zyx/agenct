//! Pure authorization decisions. No persistence, model output, or side effects.
//! Shared task cancellation follows the user-approved stage and reason policy.
use crate::Error;
use axum::http::StatusCode;

#[derive(Clone, Copy)]
pub(crate) enum Action<'a> {
    Task(&'a str),
    Proposal(&'a str),
    WorkerAccess,
    WorkerEnqueue,
    WorkerExecute,
    WorkerCancel,
}
pub(crate) enum Resource<'a> {
    Task {
        requester: &'a str,
        owner: &'a str,
        status: &'a str,
        revision: i64,
        expected_revision: i64,
    },
    Proposal {
        requester: &'a str,
        owner: &'a str,
        status: &'a str,
        revision: i64,
        expected_revision: i64,
        recipient_available: bool,
    },
    WorkerQueue {
        owner: &'a str,
        confirmed: bool,
        scope: Option<QueueScope<'a>>,
        expected_revision: i64,
        repository: &'a str,
        base_commit: &'a str,
    },
    WorkerTask {
        owner: &'a str,
        status: &'a str,
        revision: i64,
        expected_revision: i64,
    },
    WorkerJob {
        owner: &'a str,
        status: &'a str,
        task_id: &'a str,
        expected_task_id: &'a str,
    },
}
pub(crate) struct QueueScope<'a> {
    pub owner: &'a str,
    pub revision: i64,
    pub repository: &'a str,
    pub base_commit: &'a str,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Allow,
    Transition(&'static str),
    Proposal { status: &'static str, replay: bool },
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Denial {
    Hidden,
    TaskRevision,
    TaskState,
    CancellationReason,
    ProposalAction,
    ProposalRevision,
    RecipientUnavailable,
    QueueConfirmation,
    QueueScope,
}
pub(crate) type Decision = std::result::Result<Effect, Denial>;
impl From<Denial> for Error {
    fn from(value: Denial) -> Self {
        let (status, text) = match value {
            Denial::Hidden => (StatusCode::NOT_FOUND, "not found"),
            Denial::TaskRevision => (
                StatusCode::CONFLICT,
                "task changed; refresh and review before acting",
            ),
            Denial::CancellationReason => (
                StatusCode::BAD_REQUEST,
                "cancellation requires a shared reason of 1..500 characters",
            ),
            Denial::TaskState => (
                StatusCode::CONFLICT,
                "action not allowed for identity or current state",
            ),
            Denial::ProposalAction => (StatusCode::BAD_REQUEST, "unsupported proposal action"),
            Denial::ProposalRevision => {
                (StatusCode::CONFLICT, "proposal changed; refresh and review")
            }
            Denial::RecipientUnavailable => {
                (StatusCode::BAD_REQUEST, "recipient is no longer available")
            }
            Denial::QueueConfirmation => (
                StatusCode::BAD_REQUEST,
                "explicit queue-only confirmation required",
            ),
            Denial::QueueScope => (
                StatusCode::CONFLICT,
                "request does not match operator-approved task scope",
            ),
        };
        Error(status, text)
    }
}
#[derive(Clone, Copy)]
enum Role {
    Owner,
    Requester,
}
enum State {
    Exact(&'static str),
    OneOf(&'static [&'static str]),
}
struct Transition {
    action: &'static str,
    from: State,
    role: Role,
    to: &'static str,
}
const TASK_TRANSITIONS: &[Transition] = &[
    Transition {
        action: "accept",
        from: State::Exact("awaiting_owner"),
        role: Role::Owner,
        to: "working",
    },
    Transition {
        action: "deliver",
        from: State::Exact("working"),
        role: Role::Owner,
        to: "ready_for_acceptance",
    },
    Transition {
        action: "approve",
        from: State::Exact("ready_for_acceptance"),
        role: Role::Requester,
        to: "completed",
    },
    Transition {
        action: "return",
        from: State::Exact("ready_for_acceptance"),
        role: Role::Requester,
        to: "awaiting_owner",
    },
    Transition {
        action: "cancel",
        from: State::OneOf(&["awaiting_owner", "working", "ready_for_acceptance"]),
        role: Role::Requester,
        to: "canceled",
    },
    Transition {
        action: "cancel",
        from: State::OneOf(&["awaiting_owner", "working"]),
        role: Role::Owner,
        to: "canceled",
    },
];
const PROPOSAL_TRANSITIONS: &[(&str, &str)] = &[("confirm", "confirmed"), ("reject", "rejected")];
const CANCELABLE_JOBS: &[&str] = &["queued", "running", "review_ready", "cancelled"];

/// Explicit caller-supplied shared text only. Never derive a reason from private
/// chat, a model reply, or an implicit default. Character bound is Unicode scalar
/// count; trimming provides the one canonical value persisted in the audit.
pub(crate) fn cancellation_reason(reason: Option<&str>) -> Result<&str, Denial> {
    reason
        .map(str::trim)
        .filter(|r| !r.is_empty() && r.chars().count() <= 500)
        .ok_or(Denial::CancellationReason)
}

pub(crate) fn can(actor: &str, action: Action<'_>, resource: &Resource<'_>) -> Decision {
    match (action, resource) {
        (
            Action::Task(action),
            Resource::Task {
                requester,
                owner,
                status,
                revision,
                expected_revision,
            },
        ) => {
            if actor != *requester && actor != *owner {
                return Err(Denial::Hidden);
            }
            if revision != expected_revision {
                return Err(Denial::TaskRevision);
            }
            TASK_TRANSITIONS
                .iter()
                .find(|rule| {
                    rule.action == action
                        && match rule.from {
                            State::Exact(from) => *status == from,
                            State::OneOf(states) => states.contains(status),
                        }
                        && match rule.role {
                            Role::Owner => actor == *owner,
                            Role::Requester => actor == *requester,
                        }
                })
                .map(|rule| Effect::Transition(rule.to))
                .ok_or(Denial::TaskState)
        }
        (
            Action::Proposal(action),
            Resource::Proposal {
                requester,
                owner,
                status,
                revision,
                expected_revision,
                recipient_available,
            },
        ) => {
            let target = PROPOSAL_TRANSITIONS
                .iter()
                .find(|(name, _)| *name == action)
                .map(|(_, target)| *target)
                .ok_or(Denial::ProposalAction)?;
            if actor != *requester {
                return Err(Denial::Hidden);
            }
            if *status == target && *expected_revision == 0 && *revision == 1 {
                return Ok(Effect::Proposal {
                    status: target,
                    replay: true,
                });
            }
            if *status != "pending" || revision != expected_revision {
                return Err(Denial::ProposalRevision);
            }
            if action == "confirm" && (!recipient_available || actor == *owner) {
                return Err(Denial::RecipientUnavailable);
            }
            Ok(Effect::Proposal {
                status: target,
                replay: false,
            })
        }
        (Action::WorkerAccess, Resource::WorkerTask { owner, .. }) => {
            if actor == *owner {
                Ok(Effect::Allow)
            } else {
                Err(Denial::Hidden)
            }
        }
        (
            Action::WorkerEnqueue,
            Resource::WorkerQueue {
                owner,
                confirmed,
                scope,
                expected_revision,
                repository,
                base_commit,
            },
        ) => {
            if actor != *owner {
                return Err(Denial::Hidden);
            }
            if !confirmed {
                return Err(Denial::QueueConfirmation);
            }
            let scope = scope
                .as_ref()
                .filter(|s| s.owner == actor)
                .ok_or(Denial::Hidden)?;
            if scope.revision != *expected_revision
                || scope.repository != *repository
                || scope.base_commit != *base_commit
            {
                return Err(Denial::QueueScope);
            }
            Ok(Effect::Allow)
        }
        (
            Action::WorkerExecute,
            Resource::WorkerTask {
                owner,
                status,
                revision,
                expected_revision,
            },
        ) => {
            if actor == *owner && *status == "working" && revision == expected_revision {
                Ok(Effect::Allow)
            } else {
                Err(Denial::Hidden)
            }
        }
        (
            Action::WorkerCancel,
            Resource::WorkerJob {
                owner,
                status,
                task_id,
                expected_task_id,
            },
        ) => {
            if actor == *owner && task_id == expected_task_id && CANCELABLE_JOBS.contains(status) {
                Ok(Effect::Allow)
            } else {
                Err(Denial::Hidden)
            }
        }
        _ => Err(Denial::Hidden),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_identity_state_action_revision_matrix_enforces_approved_table() {
        for actor in ["alice", "bob", "eve"] {
            for state in [
                "awaiting_owner",
                "working",
                "ready_for_acceptance",
                "completed",
                "canceled",
                "legacy_unknown",
            ] {
                for action in [
                    "accept", "deliver", "approve", "return", "cancel", "unknown",
                ] {
                    for reviewed in [0, 1] {
                        let expected = if actor == "eve" {
                            Err(Denial::Hidden)
                        } else if reviewed != 1 {
                            Err(Denial::TaskRevision)
                        } else {
                            let next = match (action, state, actor) {
                                ("accept", "awaiting_owner", "bob") => Some("working"),
                                ("deliver", "working", "bob") => Some("ready_for_acceptance"),
                                ("approve", "ready_for_acceptance", "alice") => Some("completed"),
                                ("return", "ready_for_acceptance", "alice") => {
                                    Some("awaiting_owner")
                                }
                                ("cancel", "awaiting_owner" | "working", "alice" | "bob")
                                | ("cancel", "ready_for_acceptance", "alice") => Some("canceled"),
                                _ => None,
                            };
                            next.map(Effect::Transition).ok_or(Denial::TaskState)
                        };
                        assert_eq!(
                            can(
                                actor,
                                Action::Task(action),
                                &Resource::Task {
                                    requester: "alice",
                                    owner: "bob",
                                    status: state,
                                    revision: 1,
                                    expected_revision: reviewed
                                }
                            ),
                            expected,
                            "{actor}/{state}/{action}/{reviewed}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn proposal_matrix_preserves_private_replay_and_recipient_checks() {
        for actor in ["alice", "bob", "eve"] {
            for state in ["pending", "confirmed", "rejected", "unknown"] {
                for action in ["confirm", "reject", "unknown"] {
                    for revision in [0, 1, 2] {
                        for reviewed in [0, 1, 2] {
                            for available in [false, true] {
                                let target = if action == "confirm" {
                                    "confirmed"
                                } else {
                                    "rejected"
                                };
                                let expected = if action == "unknown" {
                                    Err(Denial::ProposalAction)
                                } else if actor != "alice" {
                                    Err(Denial::Hidden)
                                } else if state == target && revision == 1 && reviewed == 0 {
                                    Ok(Effect::Proposal {
                                        status: target,
                                        replay: true,
                                    })
                                } else if state != "pending" || revision != reviewed {
                                    Err(Denial::ProposalRevision)
                                } else if action == "confirm" && !available {
                                    Err(Denial::RecipientUnavailable)
                                } else {
                                    Ok(Effect::Proposal {
                                        status: target,
                                        replay: false,
                                    })
                                };
                                assert_eq!(
                                    can(
                                        actor,
                                        Action::Proposal(action),
                                        &Resource::Proposal {
                                            requester: "alice",
                                            owner: "bob",
                                            status: state,
                                            revision,
                                            expected_revision: reviewed,
                                            recipient_available: available
                                        }
                                    ),
                                    expected
                                );
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(
            can(
                "alice",
                Action::Proposal("confirm"),
                &Resource::Proposal {
                    requester: "alice",
                    owner: "alice",
                    status: "pending",
                    revision: 0,
                    expected_revision: 0,
                    recipient_available: true
                }
            ),
            Err(Denial::RecipientUnavailable)
        );
    }
    #[test]
    fn worker_matrix_has_no_requester_or_implicit_execution_authority() {
        for actor in ["alice", "bob", "eve"] {
            for state in [
                "awaiting_owner",
                "working",
                "ready_for_acceptance",
                "completed",
                "canceled",
            ] {
                for reviewed in [0, 1] {
                    let resource = Resource::WorkerTask {
                        owner: "bob",
                        status: state,
                        revision: 1,
                        expected_revision: reviewed,
                    };
                    assert_eq!(
                        can(actor, Action::WorkerAccess, &resource).is_ok(),
                        actor == "bob"
                    );
                    assert_eq!(
                        can(actor, Action::WorkerExecute, &resource).is_ok(),
                        actor == "bob" && state == "working" && reviewed == 1
                    );
                }
            }
            for status in [
                "queued",
                "running",
                "review_ready",
                "cancelled",
                "failed",
                "unknown",
            ] {
                for task in ["task", "other"] {
                    assert_eq!(
                        can(
                            actor,
                            Action::WorkerCancel,
                            &Resource::WorkerJob {
                                owner: "bob",
                                status,
                                task_id: "task",
                                expected_task_id: task
                            }
                        )
                        .is_ok(),
                        actor == "bob"
                            && task == "task"
                            && status != "failed"
                            && status != "unknown"
                    );
                }
            }
            for confirmed in [false, true] {
                for scope_owner in [None, Some("alice"), Some("bob")] {
                    for revision in [0, 1] {
                        for repository in ["repo", "other"] {
                            for base in ["commit", "other"] {
                                let expected = actor == "bob"
                                    && confirmed
                                    && scope_owner == Some("bob")
                                    && revision == 1
                                    && repository == "repo"
                                    && base == "commit";
                                assert_eq!(
                                    can(
                                        actor,
                                        Action::WorkerEnqueue,
                                        &Resource::WorkerQueue {
                                            owner: "bob",
                                            confirmed,
                                            scope: scope_owner.map(|owner| QueueScope {
                                                owner,
                                                revision: 1,
                                                repository: "repo",
                                                base_commit: "commit"
                                            }),
                                            expected_revision: revision,
                                            repository,
                                            base_commit: base
                                        }
                                    )
                                    .is_ok(),
                                    expected
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn unrelated_action_resource_pairs_fail_closed() {
        let r = Resource::WorkerTask {
            owner: "bob",
            status: "working",
            revision: 1,
            expected_revision: 1,
        };
        for action in [
            Action::Task("cancel"),
            Action::Proposal("confirm"),
            Action::WorkerEnqueue,
            Action::WorkerCancel,
        ] {
            assert_eq!(can("bob", action, &r), Err(Denial::Hidden));
        }
    }
}
