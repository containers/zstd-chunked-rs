//! Extracts a zstd:chunked file to stdout one chunk at a time
//! Should produce the exact same output as `zstdcat` on the same file

use core::ops::Range;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// A zstd:chunked file to extract
    filename: String,
}

use zstd_chunked::{MetadataReferences, Stream};

fn ref_from_slice<'a>(data: &'a [u8], range: &Range<u64>) -> Result<&'a [u8]> {
    let start = usize::try_from(range.start)?;
    let end = usize::try_from(range.end)?;
    data.get(start..end).context("Out of range!")
}

fn print_zstd_chunked(data: &[u8]) -> Result<()> {
    let references = MetadataReferences::from_footer(data)
        .context("This doesn't appear to be a zstd:chunked file")?;

    let stream = Stream::new_from_frames(
        ref_from_slice(data, &references.manifest.range)?,
        ref_from_slice(data, &references.tarsplit.range)?,
    )?;

    stream.write_to(&mut std::io::stdout(), |reference| {
        Ok(zstd::decode_all(ref_from_slice(data, &reference.range)?)?)
    })?;

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let content = std::fs::read(args.filename).context("Unable to open file")?;
    print_zstd_chunked(&content).context("Failed to process zstd:chunked file?")
}
