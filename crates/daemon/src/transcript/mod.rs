//! The Claude transcript tailer (`docs/design/c1-claude-adapter.md` D10, D13, D14; C1-8 to
//! C1-10): one [`TranscriptTailer`] per live, bound Claude launch with a transcript path. It
//! polls the file every second on a blocking task, stores assistant text and lines it cannot
//! interpret with their cursor through `append_transcript`, and starts a new generation
//! whenever continuity is disproved.

mod acc;
mod claude;
mod json;
mod read;
mod tailer;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use read::{BATCH_LINES, DROPPED_OFF_BLOCKING, FINAL_TRIES};
pub(crate) use tailer::{Mode, TAIL_POLL, Tailers};
