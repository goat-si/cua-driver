// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Shared write classification and pure post-action comparison.
//!
//! Native adapters map their own write errors to [`Reason`]; read-side retry
//! classifications must never be used for writes (agent-desktop CONCEPTS.md:203-206).
//!
//! Reference rule | Class
//! --- | ---
//! agent-desktop/crates/macos/src/actions/ax_mutation.rs:28-34 | success: delivered; action unsupported / not implemented: fall through
//! agent-desktop/crates/macos/src/actions/ax_mutation.rs:35-53,120-136 | failure: uncertain; attribute unsupported / no value: fall through only for set-attribute; perform requires observed effect or is uncertain
//! agent-desktop/crates/macos/src/actions/ax_mutation.rs:54-103 | disabled API: permission denied; invalid element: stale; illegal argument: invalid; cannot complete / unknown: uncertain
//! agent-desktop/crates/windows/src/actions/mutation.rs:44-66,127-139 | absent pattern / exhaustion / unsupported: fall through; unknown sentinel: uncertain
//! agent-desktop/crates/windows/src/actions/mutation.rs:67-96 | denied / stale / invalid / disabled: stop, not delivered
//! agent-desktop/crates/windows/src/actions/mutation.rs:97-124 | transport / timeout / unknown: uncertain
//! agent-desktop/crates/linux/src/actions/write/mutation.rs:119-139,155-190 | unsupported: fall through; absent action only for do-action; dead or unconfirmed target: stop, not delivered; timeout / uncertain: uncertain. Missing object/interface falls through only when corroborated alive.
//!
//! Adapters own fresh reads, target binding and settling. Nothing here performs
//! I/O or upgrades an unbound observation into evidence.

use cua_driver_contract::{ActionEffect, ErrorDisposition};
use serde::{Deserialize, Serialize};
use std::{fmt, ops::Range};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Unsupported,
    NotImplemented,
    AttributeUnsupported,
    NoValue,
    PatternExhausted,
    PermissionDenied,
    StaleElement,
    InvalidArgument,
    Disabled,
    Failure,
    CannotComplete,
    Transport,
    Timeout,
    Unknown,
    DeadTarget,
    UnconfirmedAlive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    SetAttribute,
    Perform,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Delivered,
    FallThrough(Reason),
    StopNotDelivered(Reason),
    StopUncertain(Reason),
}

impl Reason {
    /// Classify a reason already established by the adapter's native error table.
    pub fn classify(self, kind: WriteKind) -> WriteOutcome {
        use Reason::*;
        match self {
            Unsupported | NotImplemented | PatternExhausted => WriteOutcome::FallThrough(self),
            AttributeUnsupported | NoValue if kind == WriteKind::SetAttribute => {
                WriteOutcome::FallThrough(self)
            }
            AttributeUnsupported | NoValue => WriteOutcome::StopUncertain(self),
            PermissionDenied | StaleElement | InvalidArgument | Disabled | DeadTarget
            | UnconfirmedAlive => WriteOutcome::StopNotDelivered(self),
            Failure | CannotComplete | Transport | Timeout | Unknown => {
                WriteOutcome::StopUncertain(self)
            }
        }
    }

    /// Resolve an uninformative perform result only after an adapter has
    /// observed the effect freshly on the same target.
    pub fn classify_perform_with_observed_effect(self, fresh_effect: bool) -> WriteOutcome {
        if fresh_effect && matches!(self, Self::AttributeUnsupported | Self::NoValue) {
            WriteOutcome::Delivered
        } else {
            self.classify(WriteKind::Perform)
        }
    }
}

/// A lazy iterator of rung calls: requesting the next outcome executes that rung.
pub struct WriteChain;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainResult {
    outcomes: Vec<WriteOutcome>,
    side_effectful_prologue: bool,
}

impl WriteChain {
    pub fn run(
        rungs: impl IntoIterator<Item = WriteOutcome>,
        side_effectful_prologue: bool,
    ) -> ChainResult {
        let mut outcomes = Vec::new();
        for outcome in rungs {
            outcomes.push(outcome);
            if !matches!(outcome, WriteOutcome::FallThrough(_)) {
                break;
            }
        }
        ChainResult {
            outcomes,
            side_effectful_prologue,
        }
    }
}

impl ChainResult {
    pub fn outcomes(&self) -> &[WriteOutcome] {
        &self.outcomes
    }
    pub fn no_input(&self) -> bool {
        !self.side_effectful_prologue
            && self.outcomes.iter().all(|x| {
                matches!(
                    x,
                    WriteOutcome::FallThrough(_) | WriteOutcome::StopNotDelivered(_)
                )
            })
    }
    /// `fresh` asserts a read after dispatch bound to the same target.
    pub fn effect(&self, post: Postcondition, fresh: bool) -> ActionEffect {
        if self.no_input() {
            return ActionEffect::Refused;
        }
        if matches!(self.outcomes.last(), Some(WriteOutcome::StopUncertain(_))) {
            return ActionEffect::Unverifiable;
        }
        if !fresh || !matches!(self.outcomes.last(), Some(WriteOutcome::Delivered)) {
            return ActionEffect::Unverifiable;
        }
        match post {
            Postcondition::Confirmed => ActionEffect::Confirmed,
            Postcondition::Unchanged => ActionEffect::SuspectedNoop,
            Postcondition::Subset => ActionEffect::Partial,
            Postcondition::Indeterminate | Postcondition::Unavailable(_) => {
                ActionEffect::Unverifiable
            }
        }
    }
    /// An error disposition; a confirmed delivery has no error disposition.
    pub fn error_disposition(&self, post: Postcondition, fresh: bool) -> Option<ErrorDisposition> {
        if self.no_input() {
            return Some(ErrorDisposition::NOT_DELIVERED);
        }
        if matches!(self.outcomes.last(), Some(WriteOutcome::StopUncertain(_))) {
            return Some(ErrorDisposition::SUSPECTED_NOOP);
        }
        match self.effect(post, fresh) {
            ActionEffect::Confirmed => None,
            ActionEffect::SuspectedNoop => Some(ErrorDisposition::SUSPECTED_NOOP),
            ActionEffect::Partial | ActionEffect::Unverifiable
                if matches!(self.outcomes.last(), Some(WriteOutcome::Delivered)) =>
            {
                Some(ErrorDisposition::DELIVERED)
            }
            _ => Some(ErrorDisposition::SUSPECTED_NOOP),
        }
    }
}

/// Inputs are deliberately not printable or serializable: they may be secret.
pub enum Action<'a> {
    Toggle,
    Check,
    Uncheck,
    TypeText(&'a str),
    SetValue(&'a str),
}
pub enum Before<'a> {
    Checked(Option<bool>),
    Value {
        value: Option<&'a str>,
        selection: Option<Range<usize>>,
    },
}
pub enum Observed<'a> {
    Checked(Option<bool>),
    Value(Option<&'a str>),
    Unavailable(UnavailableReason),
}

/// Holds sensitive values internally and always redacts them when formatted.
pub struct Expected {
    inner: ExpectedInner,
}
enum ExpectedInner {
    Checked {
        before: Option<bool>,
        after: Option<bool>,
    },
    Value {
        before: Option<String>,
        after: Option<String>,
        insertion: Option<(String, String, String)>,
    },
}
impl fmt::Debug for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Expected([redacted])")
    }
}

pub fn expected_after(action: Action<'_>, before: Before<'_>) -> Expected {
    let inner = match (action, before) {
        (Action::Toggle, Before::Checked(before)) => ExpectedInner::Checked {
            before,
            after: before.map(|v| !v),
        },
        (Action::Check, Before::Checked(before)) => ExpectedInner::Checked {
            before,
            after: Some(true),
        },
        (Action::Uncheck, Before::Checked(before)) => ExpectedInner::Checked {
            before,
            after: Some(false),
        },
        (Action::SetValue(value), Before::Value { value: before, .. }) => ExpectedInner::Value {
            before: before.map(str::to_owned),
            after: Some(value.to_owned()),
            insertion: None,
        },
        (
            Action::TypeText(text),
            Before::Value {
                value: before,
                selection,
            },
        ) => {
            let parts = before.and_then(|value| {
                let range = selection.or_else(|| value.is_empty().then_some(0..0))?;
                let units: Vec<u16> = value.encode_utf16().collect();
                if range.start > range.end || range.end > units.len() {
                    return None;
                }
                Some((
                    String::from_utf16(&units[..range.start]).ok()?,
                    text.to_owned(),
                    String::from_utf16(&units[range.end..]).ok()?,
                ))
            });
            let after = parts.as_ref().map(|(a, b, c)| format!("{a}{b}{c}"));
            ExpectedInner::Value {
                before: before.map(str::to_owned),
                after,
                insertion: parts,
            }
        }
        (Action::Toggle | Action::Check | Action::Uncheck, _) => ExpectedInner::Checked {
            before: None,
            after: None,
        },
        (Action::TypeText(_) | Action::SetValue(_), _) => ExpectedInner::Value {
            before: None,
            after: None,
            insertion: None,
        },
    };
    Expected { inner }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    Secure,
    Unreadable,
    Timeout,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Postcondition {
    Confirmed,
    Unchanged,
    Subset,
    Indeterminate,
    Unavailable(UnavailableReason),
}

/// Compare values without returning their contents. `secure` applies to value
/// observations; non-value state may still be compared on a secure element.
pub fn compare(expected: &Expected, observed: Observed<'_>, secure: bool) -> Postcondition {
    match (&expected.inner, observed) {
        (ExpectedInner::Value { .. }, _) if secure => {
            Postcondition::Unavailable(UnavailableReason::Secure)
        }
        (_, Observed::Unavailable(reason)) => Postcondition::Unavailable(reason),
        (ExpectedInner::Checked { before, after }, Observed::Checked(Some(value))) => {
            if *after == Some(value) {
                Postcondition::Confirmed
            } else if *before == Some(value) {
                Postcondition::Unchanged
            } else {
                Postcondition::Indeterminate
            }
        }
        (
            ExpectedInner::Value {
                before,
                after,
                insertion,
            },
            Observed::Value(Some(value)),
        ) => {
            if after.as_deref() == Some(value) {
                Postcondition::Confirmed
            } else if before.as_deref() == Some(value) {
                Postcondition::Unchanged
            } else if insertion.as_ref().is_some_and(|(prefix, text, suffix)| {
                value
                    .strip_prefix(prefix.as_str())
                    .and_then(|v| v.strip_suffix(suffix.as_str()))
                    .is_some_and(|part| {
                        !part.is_empty() && part != text.as_str() && text.starts_with(part)
                    })
            }) {
                Postcondition::Subset
            } else {
                Postcondition::Indeterminate
            }
        }
        (_, Observed::Checked(None) | Observed::Value(None)) => {
            Postcondition::Unavailable(UnavailableReason::Unreadable)
        }
        _ => Postcondition::Indeterminate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn classifier_table_is_write_kind_sensitive() {
        use Reason::*;
        for reason in [Unsupported, NotImplemented, PatternExhausted] {
            for kind in [
                WriteKind::SetAttribute,
                WriteKind::Perform,
                WriteKind::Other,
            ] {
                assert_eq!(reason.classify(kind), WriteOutcome::FallThrough(reason));
            }
        }
        for reason in [AttributeUnsupported, NoValue] {
            assert_eq!(
                reason.classify(WriteKind::SetAttribute),
                WriteOutcome::FallThrough(reason)
            );
            for kind in [WriteKind::Perform, WriteKind::Other] {
                assert_eq!(reason.classify(kind), WriteOutcome::StopUncertain(reason));
            }
            assert_eq!(
                reason.classify_perform_with_observed_effect(true),
                WriteOutcome::Delivered
            );
            assert_eq!(
                reason.classify_perform_with_observed_effect(false),
                WriteOutcome::StopUncertain(reason)
            );
        }
        for reason in [
            PermissionDenied,
            StaleElement,
            InvalidArgument,
            Disabled,
            DeadTarget,
            UnconfirmedAlive,
        ] {
            assert_eq!(
                reason.classify(WriteKind::Other),
                WriteOutcome::StopNotDelivered(reason)
            );
        }
        for reason in [Failure, CannotComplete, Transport, Timeout, Unknown] {
            assert_eq!(
                reason.classify(WriteKind::Other),
                WriteOutcome::StopUncertain(reason)
            );
        }
    }

    #[test]
    fn chain_runs_only_until_first_non_fallthrough() {
        let absent = WriteOutcome::FallThrough(Reason::Unsupported);
        for terminal in [
            WriteOutcome::Delivered,
            WriteOutcome::StopNotDelivered(Reason::Disabled),
            WriteOutcome::StopUncertain(Reason::Timeout),
        ] {
            let calls = Cell::new(0);
            let chain = WriteChain::run(
                [absent, terminal, absent].into_iter().inspect(|_| {
                    calls.set(calls.get() + 1);
                }),
                false,
            );
            assert_eq!(calls.get(), 2);
            assert_eq!(chain.outcomes(), &[absent, terminal]);
            assert_eq!(
                chain.no_input(),
                matches!(terminal, WriteOutcome::StopNotDelivered(_))
            );
            assert!(!WriteChain::run([absent, terminal], true).no_input());
        }
        assert!(WriteChain::run([absent, absent], false).no_input());
        assert!(!WriteChain::run([absent, absent], true).no_input());
    }

    #[test]
    fn toggle_direction_is_fixed_before_dispatch() {
        let toggle = expected_after(Action::Toggle, Before::Checked(Some(true)));
        assert_eq!(
            compare(&toggle, Observed::Checked(Some(false)), false),
            Postcondition::Confirmed
        );
        assert_eq!(
            compare(&toggle, Observed::Checked(Some(true)), false),
            Postcondition::Unchanged
        );
        let missing = expected_after(Action::Toggle, Before::Checked(None));
        assert_eq!(
            compare(&missing, Observed::Checked(Some(true)), false),
            Postcondition::Indeterminate
        );
        for (action, expected) in [(Action::Check, true), (Action::Uncheck, false)] {
            let target = expected_after(action, Before::Checked(Some(!expected)));
            assert_eq!(
                compare(&target, Observed::Checked(Some(expected)), true),
                Postcondition::Confirmed
            );
            assert_eq!(
                compare(&target, Observed::Checked(Some(!expected)), true),
                Postcondition::Unchanged
            );
        }
    }

    #[test]
    fn text_replaces_utf16_selection_and_rejects_invalid_boundaries() {
        let make = |selection| {
            expected_after(
                Action::TypeText("XYZ"),
                Before::Value {
                    value: Some("a😀b"),
                    selection,
                },
            )
        };
        let target = make(Some(1..3));
        assert_eq!(
            compare(&target, Observed::Value(Some("aXYZb")), false),
            Postcondition::Confirmed
        );
        assert_eq!(
            compare(&target, Observed::Value(Some("a😀b")), false),
            Postcondition::Unchanged
        );
        assert_eq!(
            compare(&target, Observed::Value(Some("aXYb")), false),
            Postcondition::Subset
        );
        assert_eq!(
            compare(&target, Observed::Value(Some("aXZb")), false),
            Postcondition::Indeterminate
        );
        for range in [
            Some(1..2),
            Some(2..3),
            Some(Range { start: 3, end: 1 }),
            Some(0..9),
            None,
        ] {
            assert_eq!(
                compare(&make(range), Observed::Value(Some("aXYZb")), false),
                Postcondition::Indeterminate
            );
        }
        let empty = expected_after(
            Action::TypeText("ok"),
            Before::Value {
                value: Some(""),
                selection: None,
            },
        );
        assert_eq!(
            compare(&empty, Observed::Value(Some("ok")), false),
            Postcondition::Confirmed
        );
        let set = expected_after(
            Action::SetValue("exact"),
            Before::Value {
                value: Some("old"),
                selection: None,
            },
        );
        assert_eq!(
            compare(&set, Observed::Value(Some("exact")), false),
            Postcondition::Confirmed
        );
        assert_eq!(
            compare(&set, Observed::Value(Some("exac")), false),
            Postcondition::Indeterminate
        );
        assert_eq!(
            compare(&set, Observed::Value(None), false),
            Postcondition::Unavailable(UnavailableReason::Unreadable)
        );
        assert_eq!(
            compare(
                &set,
                Observed::Unavailable(UnavailableReason::Timeout),
                false
            ),
            Postcondition::Unavailable(UnavailableReason::Timeout)
        );
    }

    #[test]
    fn secure_values_are_redacted_from_all_returned_formats() {
        let expected = expected_after(
            Action::TypeText("attempted-secret"),
            Before::Value {
                value: Some("before-secret"),
                selection: Some(0..0),
            },
        );
        let verdict = compare(&expected, Observed::Value(Some("observed-secret")), true);
        assert_eq!(
            verdict,
            Postcondition::Unavailable(UnavailableReason::Secure)
        );
        let detail = format!(
            "{expected:?} {verdict:?} {}",
            serde_json::to_string(&verdict).unwrap()
        );
        for secret in ["attempted-secret", "before-secret", "observed-secret"] {
            assert!(!detail.contains(secret));
        }
    }

    #[test]
    fn chain_effect_and_disposition_require_whole_chain_and_fresh_evidence() {
        let delivered = WriteChain::run([WriteOutcome::Delivered], false);
        for (post, effect, disposition) in [
            (Postcondition::Confirmed, ActionEffect::Confirmed, None),
            (
                Postcondition::Unchanged,
                ActionEffect::SuspectedNoop,
                Some(ErrorDisposition::SUSPECTED_NOOP),
            ),
            (
                Postcondition::Subset,
                ActionEffect::Partial,
                Some(ErrorDisposition::DELIVERED),
            ),
            (
                Postcondition::Indeterminate,
                ActionEffect::Unverifiable,
                Some(ErrorDisposition::DELIVERED),
            ),
            (
                Postcondition::Unavailable(UnavailableReason::Timeout),
                ActionEffect::Unverifiable,
                Some(ErrorDisposition::DELIVERED),
            ),
        ] {
            assert_eq!(delivered.effect(post, true), effect);
            assert_eq!(delivered.error_disposition(post, true), disposition);
            assert_eq!(delivered.effect(post, false), ActionEffect::Unverifiable);
        }
        let uncertain = WriteChain::run([WriteOutcome::StopUncertain(Reason::Timeout)], false);
        assert_eq!(
            uncertain.effect(Postcondition::Confirmed, true),
            ActionEffect::Unverifiable
        );
        assert_eq!(
            uncertain.error_disposition(Postcondition::Confirmed, true),
            Some(ErrorDisposition::SUSPECTED_NOOP)
        );
        let refused = WriteChain::run(
            [
                WriteOutcome::FallThrough(Reason::Unsupported),
                WriteOutcome::StopNotDelivered(Reason::Disabled),
            ],
            false,
        );
        assert_eq!(
            refused.effect(Postcondition::Confirmed, true),
            ActionEffect::Refused
        );
        assert_eq!(
            refused.error_disposition(Postcondition::Confirmed, true),
            Some(ErrorDisposition::NOT_DELIVERED)
        );
        let prologue = WriteChain::run([WriteOutcome::StopNotDelivered(Reason::Disabled)], true);
        assert_eq!(
            prologue.effect(Postcondition::Confirmed, true),
            ActionEffect::Unverifiable
        );
        assert_eq!(
            prologue.error_disposition(Postcondition::Confirmed, true),
            Some(ErrorDisposition::SUSPECTED_NOOP)
        );
    }
}
