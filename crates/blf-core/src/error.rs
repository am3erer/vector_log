use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlfError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("BLF format error: {0}")]
    Format(String),

    #[error("BLF file is truncated")]
    Truncated,

    #[error("compression error: {0}")]
    Compression(String),

    #[error("unsupported object type: {0}")]
    UnsupportedObject(u32),

    #[error("unsupported object header version: {0}")]
    UnsupportedHeaderVersion(u16),

    #[error("incomplete object, need more bytes")]
    Incomplete,
}

pub type Result<T> = std::result::Result<T, BlfError>;
