use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as b64;
use serde::{
    Deserialize,
    de::{self, Deserializer},
};
use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned,
    little_endian::{U32, U64},
};

// "tarsplit" file format
#[derive(Debug, Deserialize)]
pub struct TarSplitEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_option_base64")]
    pub payload: Option<Box<[u8]>>,
}

fn deserialize_option_base64<'de, D>(deserializer: D) -> Result<Option<Box<[u8]>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    opt.map_or_else(
        || Ok(None),
        |s| {
            b64.decode(&s)
                .map(Vec::into_boxed_slice)
                .map(Some)
                .map_err(de::Error::custom)
        },
    )
}

// "manifest" file format
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub entries: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestEntry {
    pub name: String,
    pub size: Option<u64>,
    pub digest: Option<String>,
    pub offset: Option<u64>,
    #[serde(rename = "endOffset")]
    pub end_offset: Option<u64>,
}

// Footer
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Unaligned, KnownLayout, Immutable)]
pub struct FooterReference {
    pub offset: U64,
    pub length_compressed: U64,
    pub length_uncompressed: U64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes, Unaligned, KnownLayout, Immutable)]
pub struct Footer {
    pub(crate) skippable_magic: [u8; 4],
    pub(crate) skippable_size: U32,

    pub(crate) manifest: FooterReference,
    pub(crate) manifest_type: U64,
    pub(crate) tarsplit: FooterReference,

    pub(crate) zstd_chunked_magic: [u8; 8],
}

const ZSTD_SKIPPABLE_MAGIC: [u8; 4] = [0x50, 0x2a, 0x4d, 0x18];
const ZSTD_CHUNKED_FOOTER_SIZE: u32 = 64;
const ZSTD_CHUNKED_MANIFEST_TYPE: u64 = 1;
const ZSTD_CHUNKED_MAGIC: [u8; 8] = *b"GNUlInUx";

impl Footer {
    fn valid(&self) -> bool {
        self.skippable_magic == ZSTD_SKIPPABLE_MAGIC
            && self.skippable_size == ZSTD_CHUNKED_FOOTER_SIZE
            && self.manifest_type == ZSTD_CHUNKED_MANIFEST_TYPE
            && self.zstd_chunked_magic == ZSTD_CHUNKED_MAGIC
    }

    /// Tries to extract a zstd:chunked footer from the passed slice.  The slice can be the entire
    /// file or some portion of the end of it, but should be at least 72 bytes in length.
    pub fn from_suffix(data: &[u8]) -> Option<&Self> {
        let (_rest, footer) = Self::ref_from_suffix(data).ok()?;
        if footer.valid() { Some(footer) } else { None }
    }
}
