//! Cover art for Meowtify: a bounded decoder for untrusted images, and the disk cache that
//! keeps the downloaded bytes.

mod decode;
mod disk;

pub use decode::{decode_square, CacheError, DecodedImage};
pub use disk::DiskCache;
