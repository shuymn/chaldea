//! `ClaudeDecoder` (C1 D4, D7): the [`HookDecoder`] for `Agent::Claude`. [`parse`] reads one
//! hook's name and JSON into a [`Parsed`], purely (its input and local state only); it runs
//! inside `lifecycle::recoverable`, so a panic there keeps the body as `raw_unknown`
//! `schema_mismatch` and stops nothing. The rest builds the adapter's own events.
//!
//! | Hook | Fact | Event |
//! |---|---|---|
//! | `SessionStart` | `SessionStart` | — |
//! | `UserPromptSubmit` | `PromptSubmitted` (`prompt`) | — |
//! | `Stop` | `Stop` | — |
//! | `PermissionRequest` | `CardOpened` (`tool_name`, `tool_input`) | — |
//! | `PreToolUse` | `Other` | `tool_activity` `started` |
//! | `PostToolUse` | `CardEvidence` (`tool_name`, `tool_input`, `tool_use_id`) | `tool_activity` `completed` |
//!
//! `turn_ref` is the payload's `prompt_id` when present. A registered hook missing a field
//! its fact needs is `schema_mismatch`; any other hook is `Other`.

use chaldea_core::event::TypedEvent;
use chaldea_core::payload::{ToolActivity, ToolPhase};
use serde_json::Value;

use super::hooks::{HookDecoder, HookFact};
use crate::hook::{AcceptedHook, HookEvent};
use crate::store::{ReadTx, StoreError, prepare_content};

/// A tool call's activity, as a hook reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub phase: ToolPhase,
    pub tool_name: String,
    pub tool_use_id: Option<String>,
    pub turn_ref: Option<String>,
    pub input: Value,
    pub output: Option<Value>,
}

/// What [`parse`] read from one hook.
#[derive(Debug, Clone, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "one value per hook, returned once and taken apart at once"
)]
pub enum Parsed {
    /// The fact, and the tool activity the hook reports, if any.
    Read {
        fact: HookFact,
        activity: Option<Activity>,
    },
    /// A registered hook without a field its fact needs.
    Malformed,
}

/// The payload parser `ClaudeDecoder` runs (a test passes its own).
pub type ParseFn = fn(&str, &Value) -> Parsed;

fn str_of(j: &Value, k: &str) -> Option<String> {
    j.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// Reads one Claude hook (the table above). Pure: no store, lock, or clock.
#[must_use]
pub fn parse(event_name: &str, j: &Value) -> Parsed {
    let read = |fact| Parsed::Read {
        fact,
        activity: None,
    };
    let turn_ref = str_of(j, "prompt_id");
    let tool = || Some((str_of(j, "tool_name")?, j.get("tool_input")?.clone()));
    match event_name {
        "SessionStart" => read(HookFact::SessionStart),
        "UserPromptSubmit" => match str_of(j, "prompt") {
            Some(prompt) => read(HookFact::PromptSubmitted { prompt }),
            None => Parsed::Malformed,
        },
        "Stop" => read(HookFact::Stop),
        "PermissionRequest" => match tool() {
            Some((tool_name, tool_input)) => read(HookFact::CardOpened {
                tool_name,
                tool_input,
                turn_ref,
                correlation: None,
            }),
            None => Parsed::Malformed,
        },
        "PreToolUse" => match tool() {
            Some((tool_name, input)) => Parsed::Read {
                fact: HookFact::Other,
                activity: Some(Activity {
                    phase: ToolPhase::Started,
                    tool_name,
                    tool_use_id: str_of(j, "tool_use_id"),
                    turn_ref,
                    input,
                    output: None,
                }),
            },
            None => Parsed::Malformed,
        },
        "PostToolUse" => match tool() {
            Some((tool_name, input)) => {
                let tool_use_id = str_of(j, "tool_use_id");
                Parsed::Read {
                    fact: HookFact::CardEvidence {
                        tool_name: tool_name.clone(),
                        tool_input: input.clone(),
                        tool_use_id: tool_use_id.clone(),
                    },
                    activity: Some(Activity {
                        phase: ToolPhase::Completed,
                        tool_name,
                        tool_use_id,
                        turn_ref,
                        input,
                        output: Some(j.get("tool_response").cloned().unwrap_or(Value::Null)),
                    }),
                }
            }
            None => Parsed::Malformed,
        },
        _ => read(HookFact::Other),
    }
}

/// The Claude decoder: [`parse`] in the recovered region, then its `tool_activity` event.
#[derive(Debug, Clone, Copy)]
pub struct ClaudeDecoder {
    parse: ParseFn,
}

impl Default for ClaudeDecoder {
    fn default() -> Self {
        ClaudeDecoder { parse }
    }
}

impl ClaudeDecoder {
    /// A decoder running `parse` in place of [`parse`] (tests).
    #[doc(hidden)]
    #[must_use]
    pub fn with_parse(parse: ParseFn) -> ClaudeDecoder {
        ClaudeDecoder { parse }
    }

    /// The fact and the activity of `event_name` with `json`: [`parse`] under
    /// `lifecycle::recoverable`, a caught panic or a malformed hook as `SchemaMismatch`.
    #[must_use]
    pub fn read(&self, event_name: &str, json: &Value) -> (HookFact, Option<Activity>) {
        let parse = self.parse;
        match crate::lifecycle::recoverable(|| parse(event_name, json)) {
            Ok(Parsed::Read { fact, activity }) => (fact, activity),
            Ok(Parsed::Malformed) | Err(_) => (HookFact::SchemaMismatch, None),
        }
    }
}

/// `v` as JSON content.
fn json_content(
    v: &Value,
) -> Result<(chaldea_core::payload::Content, Vec<crate::store::BlobInput>), StoreError> {
    let bytes = serde_json::to_vec(v).map_err(|e| StoreError::Decode(e.to_string()))?;
    let p = prepare_content(bytes, "application/json").map_err(StoreError::Content)?;
    Ok((p.content, p.blob.into_iter().collect()))
}

/// The `tool_activity` event of `a`, named `tool_activity`.
pub(crate) fn activity_event(a: Activity) -> Result<HookEvent, StoreError> {
    let (input, mut blobs) = json_content(&a.input)?;
    let output = match &a.output {
        Some(o) => {
            let (c, b) = json_content(o)?;
            blobs.extend(b);
            Some(c)
        }
        None => None,
    };
    Ok(HookEvent {
        event: TypedEvent::ToolActivity(ToolActivity {
            phase: a.phase,
            tool_name: a.tool_name,
            tool_use_id: a.tool_use_id,
            turn_ref: a.turn_ref,
            input,
            output,
        }),
        name: "tool_activity".into(),
        annex: None,
        blobs,
    })
}

impl HookDecoder for ClaudeDecoder {
    fn decode(
        &self,
        _tx: &ReadTx<'_>,
        hook: &AcceptedHook<'_>,
    ) -> Result<(HookFact, Vec<HookEvent>), StoreError> {
        let (fact, activity) = self.read(hook.event_name, hook.json);
        let events = match activity {
            Some(a) => vec![activity_event(a)?],
            None => Vec::new(),
        };
        Ok((fact, events))
    }
}
