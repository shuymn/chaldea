//! `FakeDecoder` (send §6): a [`HookDecoder`] over FakeCli's payloads, for tests only. The
//! daemon never installs it; tests pass it through `SupervisorConfig::decoder`.

use chaldea_core::payload::ApprovalCorrelation;

use super::hooks::{HookDecoder, HookFact};
use crate::hook::{AcceptedHook, HookEvent};
use crate::store::{ReadTx, StoreError};

/// Maps `hook_event_name` to a fact: `SessionStart`, `UserPromptSubmit` (`prompt`),
/// `Stop`, `Interrupt`, `PostDecision`, `PermissionRequest` (`tool_name`, `tool_input`,
/// `tool_use_id` as the correlation), `PostToolUse` (`tool_use_id`); anything else is
/// `Other`.
#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeDecoder;

impl HookDecoder for FakeDecoder {
    fn decode(
        &self,
        _tx: &ReadTx<'_>,
        hook: &AcceptedHook<'_>,
    ) -> Result<(HookFact, Vec<HookEvent>), StoreError> {
        let j = hook.json;
        let s = |k: &str| {
            j.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let fact = match hook.event_name {
            "SessionStart" => HookFact::SessionStart,
            "UserPromptSubmit" => match s("prompt") {
                Some(prompt) => HookFact::PromptSubmitted { prompt },
                None => HookFact::Other,
            },
            "Stop" => HookFact::Stop,
            "Interrupt" => HookFact::Interrupt,
            "PostDecision" => HookFact::PostDecision,
            "PermissionRequest" => HookFact::CardOpened {
                tool_name: s("tool_name").unwrap_or_default(),
                tool_input: j.get("tool_input").cloned().unwrap_or_default(),
                turn_ref: s("turn_ref"),
                correlation: s("tool_use_id")
                    .map(|tool_use_id| ApprovalCorrelation { tool_use_id }),
            },
            "PostToolUse" => match s("tool_use_id") {
                Some(tool_use_id) => HookFact::CardEvidence { tool_use_id },
                None => HookFact::Other,
            },
            _ => HookFact::Other,
        };
        Ok((fact, Vec::new()))
    }
}
