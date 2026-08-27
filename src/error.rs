use std::fmt;

/// Errors produced by this crate.
#[derive(Debug)]
pub enum MfsError {
    Io(std::io::Error),
    /// The volume signature word (`drSigWord`) was not 0xD2D7.
    BadSignature { found: u16 },
    /// The volume is HFS (`drSigWord` 0x4244), which this crate does not support.
    UnsupportedHfs,
    /// Neither a raw MFS image nor a DiskCopy 4.2 container was recognized.
    UnknownImageFormat,
    /// The DiskCopy 4.2 container is malformed (bad magic, size, or checksum).
    Dc42(String),
    /// The MFS structures are internally inconsistent.
    CorruptVolume(String),
    FileNotFound(String),
    FileExists(String),
    /// The file or volume name is empty, too long, unmappable to MacRoman,
    /// or contains a forbidden character (`:` or NUL).
    InvalidName(String),
    /// Not enough free allocation blocks for the requested write.
    VolumeFull { needed_blocks: u32, free_blocks: u32 },
    /// The file directory region cannot hold another entry.
    DirectoryFull,
    /// The file's locked bit is set; clear it first (`set_locked`).
    FileLocked(String),
    /// The volume attribute word marks the volume as locked.
    VolumeLocked,
    /// Invalid parameters for formatting a new volume.
    InvalidGeometry(String),
}

impl fmt::Display for MfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MfsError::Io(e) => write!(f, "I/O error: {e}"),
            MfsError::BadSignature { found } => {
                write!(f, "not an MFS volume: signature {found:#06x} (expected 0xd2d7)")
            }
            MfsError::UnsupportedHfs => {
                write!(f, "HFS volume detected (signature 0x4244): HFS is not supported")
            }
            MfsError::UnknownImageFormat => {
                write!(f, "unrecognized image: neither raw MFS nor DiskCopy 4.2")
            }
            MfsError::Dc42(msg) => write!(f, "DiskCopy 4.2 container: {msg}"),
            MfsError::CorruptVolume(msg) => write!(f, "corrupt MFS volume: {msg}"),
            MfsError::FileNotFound(name) => write!(f, "file not found: {name}"),
            MfsError::FileExists(name) => write!(f, "file already exists: {name}"),
            MfsError::InvalidName(msg) => write!(f, "invalid name: {msg}"),
            MfsError::VolumeFull { needed_blocks, free_blocks } => write!(
                f,
                "volume full: need {needed_blocks} allocation blocks, {free_blocks} free"
            ),
            MfsError::DirectoryFull => write!(f, "file directory is full"),
            MfsError::FileLocked(name) => write!(f, "file is locked: {name}"),
            MfsError::VolumeLocked => write!(f, "volume is locked"),
            MfsError::InvalidGeometry(msg) => write!(f, "invalid geometry: {msg}"),
        }
    }
}

impl std::error::Error for MfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MfsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MfsError {
    fn from(e: std::io::Error) -> Self {
        MfsError::Io(e)
    }
}

impl From<binrw::Error> for MfsError {
    /// Backstop for the declarative layout readers/writers. Semantic validation
    /// lives in the wrappers around them, so in practice only genuine I/O
    /// failures (a short buffer) reach this conversion.
    fn from(e: binrw::Error) -> Self {
        match e {
            binrw::Error::Io(io) => MfsError::Io(io),
            other => MfsError::CorruptVolume(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, MfsError>;
