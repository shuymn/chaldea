//! Ack attribution (send §2.5 "Attribution") and the stored texts it compares, data only.

use chaldea_core::ids::{Agent, LaunchId, StreamId, ThreadId};
use chaldea_core::input::InputState;
use chaldea_core::input::encode::prompt_matches;
use chaldea_core::payload::{Content, Encoding, InputEncoding};
use chaldea_pty_runtime::OriginClass;

use super::probe::IntakeFactsV1;
use crate::store::{InputRow, ReadTx, StoreError};

/// The stored text of `c` on `stream`: inline UTF-8, or the blob's bytes.
///
/// # Errors
///
/// A store error, or text that is not UTF-8.
pub(crate) fn content_text(
    tx: &ReadTx<'_>,
    stream: StreamId,
    c: &Content,
) -> Result<String, StoreError> {
    match c {
        Content::Inline {
            inline,
            encoding: Encoding::Utf8,
        } => Ok(inline.clone()),
        Content::Inline { .. } => Err(StoreError::Decode("a base64 prompt".into())),
        Content::Blob { blob, .. } => {
            let body = tx
                .blob(stream, &blob.sha256)?
                .ok_or_else(|| StoreError::Decode("a prompt's blob is missing".into()))?;
            String::from_utf8(body.data).map_err(|e| StoreError::Decode(e.to_string()))
        }
    }
}

/// What the attribution decided for a prompt.
#[derive(Debug, Clone)]
pub(crate) struct Attribution {
    /// The one in-flight input of the launch whose text matches the prompt.
    pub matched: Option<InputRow>,
    /// Whether the hook acks it.
    pub attributable: bool,
}

/// The inputs of `launch` in `states` whose text matches `prompt`.
fn matching(
    tx: &ReadTx<'_>,
    thread: ThreadId,
    launch: LaunchId,
    agent: Agent,
    states: &[InputState],
    prompt: &str,
) -> Result<Vec<InputRow>, StoreError> {
    let mut out = Vec::new();
    for i in tx.inputs(thread, states)? {
        if i.launch_id != Some(launch) {
            continue;
        }
        let text = content_text(tx, StreamId::Thread(thread), &i.text)?;
        if prompt_matches(agent, i.encoding, &text, prompt) {
            out.push(i);
        }
    }
    Ok(out)
}

/// Attribution of a `PromptSubmitted` with `prompt` (send §2.5 steps 1–2).
///
/// # Errors
///
/// A store error.
pub(crate) fn attribute(
    tx: &ReadTx<'_>,
    thread: ThreadId,
    launch: LaunchId,
    agent: Agent,
    prompt: &str,
    facts: Option<&IntakeFactsV1>,
    reliable: bool,
) -> Result<Attribution, StoreError> {
    let in_flight = [
        InputState::Sending,
        InputState::Sent,
        InputState::Unconfirmed,
    ];
    let mut m = matching(tx, thread, launch, agent, &in_flight, prompt)?;
    if m.len() != 1 {
        return Ok(Attribution {
            matched: None,
            attributable: false,
        });
    }
    let x = m.remove(0);
    let attributable = reliable
        && facts.is_some_and(|f| written_alone(f, &x))
        && !earlier_match(tx, thread, launch, agent, prompt)?;
    Ok(Attribution {
        matched: Some(x),
        attributable,
    })
}

/// The facts show `x`'s unit (or, for a positional input, its argv) as the last counted
/// write before the intake.
fn written_alone(f: &IntakeFactsV1, x: &InputRow) -> bool {
    if x.encoding == InputEncoding::Positional {
        // SB5: `Human` and `Input` writes above the publication count; replies do not.
        return f.at(OriginClass::Input) <= f.reference && f.at(OriginClass::Human) <= f.reference;
    }
    let Some(d) = &f.inflight else {
        return false;
    };
    let Some(s) = d.position else {
        return false;
    };
    d.input_id == x.input_id
        && d.written
        && !d.competing
        && f.at(OriginClass::Human) < s
        && f.at(OriginClass::Reply) < s
}

/// An input of the launch that left flight without a hook ack (`acked` by the owner,
/// `dismissed`) matches the prompt: an earlier dispatch may be what the CLI submitted.
fn earlier_match(
    tx: &ReadTx<'_>,
    thread: ThreadId,
    launch: LaunchId,
    agent: Agent,
    prompt: &str,
) -> Result<bool, StoreError> {
    let left = matching(
        tx,
        thread,
        launch,
        agent,
        &[InputState::Acked, InputState::Dismissed],
        prompt,
    )?;
    Ok(left
        .iter()
        .any(|i| i.state == InputState::Dismissed || i.ack_seq.is_none()))
}
