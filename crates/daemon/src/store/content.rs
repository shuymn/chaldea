//! `Content` preparation, the typed walker over every `Content` field of an event, and the
//! preview rule (`docs/design/event-store.md` §2.5).

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chaldea_core::event::TypedEvent;
use chaldea_core::payload::{
    BlobRef, Component, Content, Encoding, INLINE_LIMIT_BYTES, PREVIEW_LIMIT_BYTES,
};
use sha2::{Digest, Sha256};

use super::write_tx::BlobInput;

/// The largest blob the store keeps (an internal limit).
pub const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// A prepared `Content` and, for a blob, the bytes to pass with the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub content: Content,
    pub blob: Option<BlobInput>,
}

/// A `Content` value or blob input that breaks §2.5's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentError {
    /// Above `MAX_BLOB_BYTES`.
    TooLarge(u64),
    /// Inline content above 64 KiB (after decoding, for base64).
    InlineTooLarge(usize),
    InvalidBase64,
    /// Base64 whose bytes are valid UTF-8: such content is `utf8`, so each content has one
    /// form.
    Base64OfUtf8,
    /// A blob reference at or below the inline limit.
    BlobTooSmall(u64),
    BadSha256(String),
    EmptyMediaType,
    /// No stored blob and no blob input with this hash.
    BlobNotFound(String),
    /// A blob reference whose `field` does not match the bytes.
    BlobMismatch {
        sha256: String,
        field: &'static str,
    },
    /// A blob input that no typed reference of the event names.
    UnreferencedBlob(String),
}

impl fmt::Display for ContentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentError::TooLarge(n) => write!(f, "{n} bytes is above the 64 MiB blob limit"),
            ContentError::InlineTooLarge(n) => {
                write!(f, "inline content of {n} bytes is above 64 KiB")
            }
            ContentError::InvalidBase64 => f.write_str("invalid base64"),
            ContentError::Base64OfUtf8 => f.write_str("base64 of valid UTF-8 must be utf8"),
            ContentError::BlobTooSmall(n) => write!(f, "a blob of {n} bytes must be inline"),
            ContentError::BadSha256(s) => write!(f, "not a lowercase hex SHA-256: {s}"),
            ContentError::EmptyMediaType => f.write_str("empty media type"),
            ContentError::BlobNotFound(s) => write!(f, "blob {s} is neither stored nor given"),
            ContentError::BlobMismatch { sha256, field } => {
                write!(f, "blob {sha256}: {field} does not match the bytes")
            }
            ContentError::UnreferencedBlob(s) => write!(f, "blob input {s} is not referenced"),
        }
    }
}

impl std::error::Error for ContentError {}

/// Lowercase hex SHA-256.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in &digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Turns bytes into `Content`: inline at or below 64 KiB (`utf8` for valid UTF-8, else
/// `base64`), a blob above it.
///
/// # Errors
///
/// `TooLarge` above [`MAX_BLOB_BYTES`].
pub fn prepare_content(bytes: Vec<u8>, media_type: &str) -> Result<Prepared, ContentError> {
    if bytes.len() > MAX_BLOB_BYTES {
        return Err(ContentError::TooLarge(bytes.len() as u64));
    }
    if bytes.len() <= INLINE_LIMIT_BYTES {
        let content = match String::from_utf8(bytes) {
            Ok(s) => Content::Inline {
                inline: s,
                encoding: Encoding::Utf8,
            },
            Err(e) => Content::Inline {
                inline: STANDARD.encode(e.as_bytes()),
                encoding: Encoding::Base64,
            },
        };
        return Ok(Prepared {
            content,
            blob: None,
        });
    }
    let (preview, preview_lossy) = preview(&bytes);
    let content = Content::Blob {
        blob: BlobRef {
            sha256: sha256_hex(&bytes),
            size: bytes.len() as u64,
            media_type: media_type.to_owned(),
        },
        preview,
        preview_lossy,
    };
    Ok(Prepared {
        content,
        blob: Some(BlobInput { data: bytes }),
    })
}

/// The bytes `preview` reads: the window plus up to 3 bytes past it.
pub(crate) const PREVIEW_READ_BYTES: usize = PREVIEW_LIMIT_BYTES + 3;

/// The preview of content whose first bytes are `head` (at least `min(len, 1027)` bytes of
/// it): the first 1024 bytes decoded as UTF-8, each invalid sequence replaced by U+FFFD
/// (setting the flag); a valid character cut by the window is left out without setting it.
pub(crate) fn preview(head: &[u8]) -> (String, bool) {
    let window = &head[..head.len().min(PREVIEW_LIMIT_BYTES)];
    let mut out = String::new();
    let mut lossy = false;
    let mut at = 0;
    while at < window.len() {
        match std::str::from_utf8(&window[at..]) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = at + e.valid_up_to();
                out.push_str(std::str::from_utf8(&window[at..valid]).unwrap_or_default());
                if let Some(n) = e.error_len() {
                    out.push('\u{FFFD}');
                    lossy = true;
                    at = valid + n;
                } else {
                    // The window ends inside a sequence. Look up to 3 bytes past it: a valid
                    // scalar there is cut only by the window and is left out.
                    if !completes_a_scalar(&head[valid..]) {
                        out.push('\u{FFFD}');
                        lossy = true;
                    }
                    break;
                }
            }
        }
    }
    (out, lossy)
}

/// Whether `bytes` starts with one complete, valid UTF-8 scalar.
fn completes_a_scalar(bytes: &[u8]) -> bool {
    let need = match bytes.first() {
        Some(b) if *b >= 0xF0 => 4,
        Some(b) if *b >= 0xE0 => 3,
        Some(b) if *b >= 0xC0 => 2,
        _ => return false,
    };
    bytes.len() >= need && std::str::from_utf8(&bytes[..need]).is_ok()
}

/// Every typed `Content` field of an event. Matches every kind without a wildcard, so a
/// new kind with a `Content` field does not compile until it is listed here; a surface's
/// grids are reached through `list` items only, never through button arguments.
pub(crate) fn contents(e: &TypedEvent) -> Vec<&Content> {
    let mut out = Vec::new();
    match e {
        TypedEvent::UserMessage(p) => out.push(&p.prompt),
        TypedEvent::AssistantMessage(p) => out.push(&p.text),
        TypedEvent::InputCreated(p) => out.push(&p.text),
        TypedEvent::ToolActivity(p) => {
            out.push(&p.input);
            if let Some(o) = &p.output {
                out.push(o);
            }
        }
        TypedEvent::ApprovalRequested(p) => out.push(&p.tool_input),
        TypedEvent::MailboxMessage(p) => out.push(&p.body),
        TypedEvent::VtSnapshotTaken(p) => out.push(&p.grid),
        TypedEvent::RawUnknown(p) => out.push(&p.raw),
        TypedEvent::SurfaceUpdate(p) => components(&p.components, &mut out),
        TypedEvent::ThreadRegistered(_)
        | TypedEvent::ThreadClosed(_)
        | TypedEvent::LaunchReserved(_)
        | TypedEvent::LaunchStarted(_)
        | TypedEvent::LaunchExited(_)
        | TypedEvent::LaunchDeathConfirmed(_)
        | TypedEvent::SessionBound(_)
        | TypedEvent::TranscriptGeneration(_)
        | TypedEvent::ReadinessObserved(_)
        | TypedEvent::HumanWrite(_)
        | TypedEvent::StatusChanged(_)
        | TypedEvent::ApprovalResolved(_)
        | TypedEvent::InputStateChanged(_)
        | TypedEvent::TaskSet(_)
        | TypedEvent::WorktreeChanged(_) => {}
    }
    out
}

fn components<'a>(cs: &'a [Component], out: &mut Vec<&'a Content>) {
    for c in cs {
        match c {
            Component::VtSnapshot { grid, .. } => out.push(grid),
            Component::List { items } => components(items, out),
            Component::Text { .. }
            | Component::Markdown { .. }
            | Component::Button { .. }
            | Component::TextInput { .. } => {}
        }
    }
}

/// Checks an inline value; blob references are checked by the caller against the bytes.
pub(crate) fn check_inline(inline: &str, encoding: Encoding) -> Result<(), ContentError> {
    match encoding {
        Encoding::Utf8 => {
            if inline.len() > INLINE_LIMIT_BYTES {
                return Err(ContentError::InlineTooLarge(inline.len()));
            }
        }
        Encoding::Base64 => {
            let bytes = STANDARD
                .decode(inline)
                .map_err(|_| ContentError::InvalidBase64)?;
            if bytes.len() > INLINE_LIMIT_BYTES {
                return Err(ContentError::InlineTooLarge(bytes.len()));
            }
            if std::str::from_utf8(&bytes).is_ok() {
                return Err(ContentError::Base64OfUtf8);
            }
        }
    }
    Ok(())
}

/// Checks a blob reference's own fields (hash form, size bounds, media type).
pub(crate) fn check_blob_ref(r: &BlobRef) -> Result<(), ContentError> {
    if r.sha256.len() != 64
        || !r
            .sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ContentError::BadSha256(r.sha256.clone()));
    }
    if r.media_type.is_empty() {
        return Err(ContentError::EmptyMediaType);
    }
    if r.size <= INLINE_LIMIT_BYTES as u64 {
        return Err(ContentError::BlobTooSmall(r.size));
    }
    if r.size > MAX_BLOB_BYTES as u64 {
        return Err(ContentError::TooLarge(r.size));
    }
    Ok(())
}

/// Checks a blob reference against the bytes it names: `size` and the head of the data.
pub(crate) fn check_against(
    r: &BlobRef,
    preview_text: &str,
    preview_lossy: bool,
    size: u64,
    head: &[u8],
) -> Result<(), ContentError> {
    let mismatch = |field| ContentError::BlobMismatch {
        sha256: r.sha256.clone(),
        field,
    };
    if r.size != size {
        return Err(mismatch("size"));
    }
    let (p, lossy) = preview(head);
    if p != preview_text {
        return Err(mismatch("preview"));
    }
    if lossy != preview_lossy {
        return Err(mismatch("preview_lossy"));
    }
    Ok(())
}
