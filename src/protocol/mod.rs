//! The cp2 peer wire protocol: message types ([`Frame`]) and the
//! length-prefixed, optionally-compressed codec ([`stream`]).
//!
//! This layer is about *messages*, not connections: the codec works over any
//! `tokio::io::AsyncRead`/`AsyncWrite` byte stream and has no knowledge of
//! SSH or any other transport (that lives in `crate::transport`). The
//! [`sync`](crate::sync) layer decides *what* to send; this layer defines
//! *how* it is serialized.

pub mod error;
pub mod frame;
pub mod stream;

pub use error::ProtocolError;
pub use frame::{
    BatchFile, BatchItem, BUILD_FINGERPRINT, FileKind, FileMeta, Frame, LinkKind, HardlinkSpec, LinkSpec, SpecialSpec,
    SignatureEntry, SkippedFile, TargetOs, VERSION_BANNER,
};
