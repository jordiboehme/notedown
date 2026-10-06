//! Error type of the parser.

/// Everything that can go wrong while opening a `.note` file.
#[derive(Debug, thiserror::Error)]
pub enum NoteError {
    /// The file could not be read.
    #[error("cannot read the file: {0}")]
    Io(#[from] std::io::Error),
    /// The bytes do not start with a Supernote signature (docs/note-format.md 1).
    #[error("not a Supernote .note file")]
    NotANote,
    /// The signature is well-formed but not in [`crate::KNOWN_SIGNATURES`] and the
    /// policy is [`crate::VersionPolicy::Strict`].
    #[error("unknown .note version {0}")]
    UnknownVersion(String),
    /// The container is damaged. The message names the key or block.
    #[error("corrupt .note file: {0}")]
    Corrupt(String),
    /// A value or block could not be decoded (base64, JSON, bitmap, ...).
    #[error("cannot decode .note data: {0}")]
    Decode(String),
}

pub(crate) fn corrupt(message: impl Into<String>) -> NoteError {
    NoteError::Corrupt(message.into())
}
