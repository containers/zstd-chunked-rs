//! A library to help read zstd:chunked files
mod format;

use core::ops::Range;
use std::{collections::HashMap, io::Write};

use anyhow::{Context, Result, ensure};

use self::format::{Footer, FooterReference, Manifest, TarSplitEntry};

/// A reference to a compressed range in a zstd:chunked file, along with size and checksum
/// information about the uncompressed data at that range.
#[derive(Debug, Clone)]
pub struct ContentReference {
    /// The range itself, in bytes, in the compressed file.
    pub range: Range<u64>,

    /// The digest of the data at the range, after decompression.
    pub digest: String,

    /// The size of the compressed data at the range, after decompression.
    pub size: u64,
}

/// A chunk of data in a zstd:chunked stream.  Either contains inline data or a reference to a
/// compressed range (and checksum and size information about the data at that range).
#[derive(Debug, Clone)]
pub enum Chunk {
    /// The literal data appears directly.
    Inline(Box<[u8]>),
    /// The data appears at the referenced range, which may need to be fetched and decompressed.
    External(Box<[ContentReference]>),
}

/// Represents the layout of a zstd:chunked file.  You can reconstruct the original file contents
/// by iterating over the chunks.
#[derive(Debug)]
pub struct Stream {
    /// The chunks in the file.
    pub chunks: Vec<Chunk>,
}

impl Stream {
    /// Create the metadata structure from the compressed frames referred to by the ranges in the
    /// OCI layer descriptor annotations or the file footer.
    ///
    /// # Errors
    ///
    /// This function can fail if any of the metadata isn't in the expected format (zstd-compressed
    /// JSON) or if there are missing mandatory fields or internal inconsistencies.  In all cases,
    /// it indicates a corrupt zstd:chunked file (or a bug in the library).
    pub fn new_from_frames(manifest: &[u8], tarsplit: &[u8]) -> Result<Self> {
        let manifest = zstd::decode_all(manifest)?;
        let manifest: Manifest = serde_json::from_slice(&manifest)?;

        ensure!(
            manifest.version == 1,
            "Incorrect zstd:chunked CRFS manifest version"
        );

        // Read the manifest entries into a table by filename, taking only the ones that have the
        // digest, size, offset and end_offset information filled in (ie: regular files).  Don't
        // handle chunks.
        let manifest_entries: HashMap<String, ContentReference> = manifest
            .entries
            .into_iter()
            .filter_map(|entry| {
                Some((
                    entry.name,
                    ContentReference {
                        digest: entry.digest?,
                        size: entry.size?,
                        range: entry.offset?..entry.end_offset?,
                    },
                ))
            })
            .collect();

        // Iterate over the chunks in the tarsplit.  For inline chunks, store the inline data.  For
        // external chunks, look them up in the manifest_entries and store what we find.
        let tarsplit = String::from_utf8(zstd::decode_all(tarsplit)?)?;
        let mut chunks = vec![];

        for line in tarsplit.lines() {
            let entry: TarSplitEntry = serde_json::from_str(line)?;

            match entry {
                TarSplitEntry {
                    name: Some(name),
                    size: Some(size),
                    ..  // ignored: crc64
                } => {
                    let reference = manifest_entries.get(&name)
                        .with_context(|| format!("Filename {name} in zstd:chunked tarsplit missing from manifest"))?;
                    ensure!(size == reference.size, "size mismatch");
                    chunks.push(Chunk::External(Box::from([reference.clone()])));
                }
                TarSplitEntry {
                    payload: Some(payload),
                    ..
                } => chunks.push(Chunk::Inline(payload)),
                _ => {}
            }
        }

        Ok(Self { chunks })
    }

    /// Iterates over all of the references that need to be satisfied for this stream to be
    /// reconstructed.  This might be useful to help prefetch the required items.
    pub fn references(&self) -> impl Iterator<Item = &ContentReference> {
        self.chunks.iter().flat_map(|chunk| {
            if let Chunk::External(items) = chunk {
                items.as_ref()
            } else {
                &[]
            }
        })
    }

    /// Writes the content of the stream to the given writer.  The `resolve_reference()` function
    /// should return the *decompressed* data corresponding to the reference.
    ///
    /// # Errors
    ///
    /// This function can fail only in response to external errors: a failure of the
    /// `resolve_reference()` function or a failure to write to the writer.
    pub fn write_to(
        &self,
        write: &mut impl Write,
        resolve_reference: impl Fn(&ContentReference) -> Result<Vec<u8>>,
    ) -> Result<()> {
        for chunk in &self.chunks {
            match chunk {
                Chunk::Inline(data) => {
                    write.write_all(data)?;
                }
                Chunk::External(refs) => {
                    for r#ref in refs {
                        write.write_all(&resolve_reference(r#ref)?)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// A reference to file metadata, either the manifest or the tarsplit
#[derive(Debug)]
pub struct MetadataReference {
    /// The range itself, in bytes, in the compressed file.
    pub range: Range<u64>,

    /// The digest of the data at the range, *before* decompression.  This will be missing if we
    /// read from the file footer or if the OCI annotations didn't provide it.
    pub digest: Option<String>,

    /// The size of the compressed data at the range, after decompression.
    pub uncompressed_size: u64,
}

impl MetadataReference {
    const fn from_footer(value: &FooterReference) -> Self {
        let start = value.offset.get();
        let end = start + value.length_compressed.get();

        Self {
            range: start..end,
            digest: None,
            uncompressed_size: value.length_uncompressed.get(),
        }
    }
}

/// References to the manifest and tarsplit metadata.  You can read these from the file footer or
/// from the annotations on the OCI layer descriptor.
#[derive(Debug)]
pub struct MetadataReferences {
    /// The location of the manifest data
    pub manifest: MetadataReference,
    /// The location of the tarsplit data
    pub tarsplit: MetadataReference,
}

fn to_vec_u64(value: &str) -> Option<Vec<u64>> {
    value.split(':').map(|s| s.parse().ok()).collect()
}

impl MetadataReferences {
    /// Read the metadata references from the file footer.  The provided data can be any suffix of
    /// the file, but it must be at least 72 bytes in length (to contain the footer).  Returns None
    /// if this doesn't appear to be a zstd:chunked file.
    #[must_use]
    pub fn from_footer(suffix: &[u8]) -> Option<Self> {
        let footer = Footer::from_suffix(suffix)?;
        Some(Self {
            manifest: MetadataReference::from_footer(&footer.manifest),
            tarsplit: MetadataReference::from_footer(&footer.tarsplit),
        })
    }

    /// Parses the metadata references from OCI layer descriptor annotations.  You should provide a
    /// 'get' closure that returns the requested annotation, or None if it doesn't exist. Returns
    /// None if this doesn't appear to be a zstd:chunked layer descriptor.
    pub fn from_oci<'a, S: AsRef<str> + 'a>(get: impl Fn(&str) -> Option<&'a S>) -> Option<Self> {
        let manifest_digest = get("io.github.containers.zstd-chunked.manifest-checksum");
        let manifest_position = get("io.github.containers.zstd-chunked.manifest-position")?;
        let tarsplit_digest = get("io.github.containers.zstd-chunked.tarsplit-checksum");
        let tarsplit_position = get("io.github.containers.zstd-chunked.tarsplit-position")?;

        Some(Self {
            manifest: match to_vec_u64(manifest_position.as_ref())?.as_slice() {
                &[start, length, uncompressed_size, 1] => MetadataReference {
                    range: start..(start + length),
                    digest: manifest_digest.map(|s| s.as_ref().to_owned()),
                    uncompressed_size,
                },
                _ => None?,
            },
            tarsplit: match to_vec_u64(tarsplit_position.as_ref())?.as_slice() {
                &[start, length, uncompressed_size] => MetadataReference {
                    range: start..(start + length),
                    digest: tarsplit_digest.map(|s| s.as_ref().to_owned()),
                    uncompressed_size,
                },
                _ => None?,
            },
        })
    }
}
