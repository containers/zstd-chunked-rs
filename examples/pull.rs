//! Pull a zstd:chunked image using oci-client
use std::{
    fmt, fs,
    ops::Range,
    path::PathBuf,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::{
    channel::oneshot,
    stream::{self, StreamExt, TryStreamExt},
    try_join,
};
use futures_timer::Delay;
use indicatif::{ProgressBar, ProgressStyle};
use oci_client::{
    Client, Reference,
    client::{BlobResponse, ClientConfig},
    manifest::{OciDescriptor, OciManifest},
    secrets::RegistryAuth,
};

use zstd_chunked::{ContentReference, MetadataReference, MetadataReferences, Stream};

#[derive(Parser, Debug)]
struct Args {
    image: Reference,
}

// The Chameleon keeps track of how well the download is going.  Each byte successfully downloaded
// increases the karma by 1 and each network failure decreases it by 1.  The passage of time also
// decreases karma, with exponential decay.  This means that as long as progress is steady,
// even with really slow download speeds (think 10bytes/sec), we can tolerate a large number of
// network errors, but once we stop making forward progress and exponential decay sets in, our
// patience for errors decreases rapidly.  It also means that a single error at the start is
// immediately fatal, which feels correct.
struct Chameleon {
    // 🌈🦎📊
    karma: f64,
    updated: Instant,
}

impl Chameleon {
    fn get(&self, now: &Instant) -> f64 {
        // first order exponential decay, time constant = 1s (ie: drops to 36.79% after 1 sec)
        self.karma / now.duration_since(self.updated).as_secs_f64().exp()
    }

    fn update(&mut self, delta: impl Into<f64>) -> f64 {
        let now = Instant::now();
        self.karma = self.get(&now) + delta.into();
        self.updated = now;
        self.karma
    }
}

impl fmt::Debug for Chameleon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Chameleon {{ value: {}, updated: {:?} }} -> {}",
            self.karma,
            self.updated,
            self.get(&Instant::now())
        )
    }
}

impl Default for Chameleon {
    fn default() -> Self {
        Self {
            karma: 0.,
            updated: Instant::now(),
        }
    }
}

struct PullOp {
    client: Client,
    cache: PathBuf,
    image: Reference,
    progress: ProgressBar,
    karma: Mutex<Chameleon>, // could be RefCell but then PullOp isn't Send
}

async fn run_in_thread(f: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    thread::spawn(move || tx.send(f()));
    rx.await.context("Thread panicked or sender dropped")?
}

impl PullOp {
    async fn softfail(&self, err: impl Into<anyhow::Error>) -> Result<()> {
        #[allow(clippy::unwrap_used)]
        if self.karma.lock().unwrap().update(-1.) < 0. {
            // Karma went negative: let the error bubble out.
            Err(err.into())
        } else {
            // Give it a second...
            Delay::new(Duration::from_secs(1)).await;
            Ok(())
        }
    }

    // To simplify progress tracking, if this function fails, the entire operation needs to be
    // aborted, so it tries really hard not to fail... it will also never download any byte that it
    // has already successfully received (ie: it will make the range request smaller before trying
    // again).
    async fn download_range(&self, desc: &OciDescriptor, range: &Range<u64>) -> Result<Vec<u8>> {
        let (mut start, end) = (range.start, range.end);
        let mut data = vec![];

        'send_request: while start < end {
            let resp = match self
                .client
                .pull_blob_stream_partial(&self.image, desc, start, Some(range.end - start))
                .await
            {
                Ok(resp) => resp,
                Err(err) => {
                    self.softfail(err).await?;
                    continue 'send_request;
                }
            };

            // Maybe some servers would respond with a full request if we give the complete range
            // but let's wait until someone actually encounters that before we try to handle it...
            let BlobResponse::Partial(mut stream) = resp else {
                bail!("Server has no range support");
            };

            while let Some(result) = stream.next().await {
                match result {
                    Ok(bytes) => {
                        let n_bytes = bytes.len() as u64;

                        #[allow(clippy::cast_precision_loss, clippy::unwrap_used)]
                        self.karma.lock().unwrap().update(n_bytes as f64);
                        data.extend_from_slice(&bytes);
                        self.progress.inc(n_bytes);
                        start += n_bytes;
                    }
                    Err(err) => {
                        self.softfail(err).await?;
                        continue 'send_request;
                    }
                }
            }
        }

        Ok(data)
    }

    async fn check_and_save(path: PathBuf, decompress: bool, mut data: Vec<u8>) -> Result<()> {
        run_in_thread(move || {
            if decompress {
                data = zstd::decode_all(&data[..])?;
            }

            // TODO: validate...
            let digest = path.file_name();
            let _ = digest;

            // write it to the path
            fs::write(&path, &data)?;
            Ok(())
        })
        .await
    }

    async fn download_metadata(
        &self,
        layer: &OciDescriptor,
        reference: &MetadataReference,
    ) -> Result<Vec<u8>> {
        if let Some(digest) = &reference.digest {
            if let Ok(data) = fs::read(self.cache.join(digest)) {
                // TODO: validate
                self.progress
                    .dec_length(reference.range.end - reference.range.start);
                return Ok(data);
            }
        }

        let result = self.download_range(layer, &reference.range).await?;

        if let Some(digest) = &reference.digest {
            // Caching metadata might not make sense for the "incremental updates" case (since it's
            // definitely going to be different next time) but it definitely makes sense from the
            // "bad network connection and my download got interrupted" case.
            Self::check_and_save(self.cache.join(digest), false, result.clone()).await?;
        }

        Ok(result)
    }

    async fn ensure_content(
        &self,
        layer: &OciDescriptor,
        reference: &ContentReference,
    ) -> Result<()> {
        let cache_path = self.cache.join(&reference.digest);
        if fs::exists(&cache_path)? {
            self.progress
                .dec_length(reference.range.end - reference.range.start);
        } else {
            let result = self.download_range(layer, &reference.range).await?;
            Self::check_and_save(cache_path, true, result).await?;
        }

        Ok(())
    }

    async fn download_zstd_chunked_layer(&self, layer: &OciDescriptor) -> Result<Stream> {
        let metadata = layer
            .annotations
            .as_ref()
            .and_then(|annotations| MetadataReferences::from_oci(|key| annotations.get(key)))
            .context("Not a zstd:chunked image?")?;

        let (manifest, tarsplit) = try_join!(
            self.download_metadata(layer, &metadata.manifest),
            self.download_metadata(layer, &metadata.tarsplit)
        )?;

        let stream = Stream::new_from_frames(&manifest[..], &tarsplit[..])?;

        // Remove the parts of the file that we know we won't need (tar headers, etc.)
        // We get that by summing up the parts we do need and subtracting it from the total size.
        let already_accounted = (manifest.len() + tarsplit.len()) as u64;
        let needed: u64 = stream
            .references()
            .map(|r| r.range.end - r.range.start)
            .sum();
        let unneeded = TryInto::<u64>::try_into(layer.size)? - needed - already_accounted;
        self.progress.dec_length(unneeded);

        stream::iter(stream.references())
            .map(Result::<_, anyhow::Error>::Ok)
            .try_for_each_concurrent(100, |reference| async move {
                self.ensure_content(layer, reference).await?;
                Ok(())
            })
            .await?;

        Ok(stream)
    }

    async fn pull(image: Reference, cache: PathBuf) -> Result<()> {
        let client = Client::new(ClientConfig {
            connect_timeout: Some(Duration::from_secs(1)),
            read_timeout: Some(Duration::from_secs(1)),
            ..Default::default()
        });

        let (manifest, _) = client
            .pull_manifest(&image, &RegistryAuth::Anonymous)
            .await?;

        let OciManifest::Image(manifest) = manifest else {
            bail!("This is not an image manifest");
        };

        let total: i64 = manifest.layers.iter().map(|l| l.size).sum();

        let progress = ProgressBar::new(total.try_into()?);
        progress.enable_steady_tick(Duration::from_millis(100));
        progress.set_style(ProgressStyle::with_template(
            "[eta {eta}] {bar:40.cyan/blue} {decimal_bytes:>7}/{decimal_total_bytes:7} {decimal_bytes_per_sec} {msg}",
        )?);

        let this = Self {
            client,
            cache,
            image,
            progress,
            karma: Chameleon::default().into(),
        };

        for layer in &manifest.layers {
            this.download_zstd_chunked_layer(layer).await?;
        }

        this.progress.finish();

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let cache = PathBuf::from("tmp");
    fs::create_dir_all(&cache)?;

    PullOp::pull(args.image, cache).await?;

    Ok(())
}
