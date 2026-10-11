//! dvravc: convert 4-channel DVR ".avc" surveillance recordings to MP4.

pub mod convert;
pub mod demux;

pub use convert::{Msg, Options, convert, ffmpeg_available, spawn_batch};
pub use demux::Recording;
