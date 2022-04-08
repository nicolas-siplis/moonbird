use again::RetryPolicy;
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures::lock::Mutex;
use futures::{stream::iter, StreamExt};
use reqwest::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};
use std::env::temp_dir;
use std::fs::{create_dir, remove_dir_all};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::time::Instant;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.file.is_none() {
        println!("No name specified, will create audio file with default space name");
    }

    Guest::new(&args.bearer)
        .await?
        .space(&args.space)
        .await?
        .download(args.file, args.concurrency)
        .await?;

    println!("\nDone");
    Ok(())
}

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// ID of the space to download
    #[clap(short, long)]
    space: String,

    /// Name for the generated audio file
    #[clap(short, long)]
    file: Option<String>,

    /// Maximum allowed amount of concurrent fragment requests while downloading space
    #[clap(short, long, default_value_t = 50)]
    concurrency: usize,

    /// Authentication token to get required metadata
    #[clap(
        short,
        long,
        default_value = "AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs=1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA"
    )]
    bearer: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Guest {
    bearer_token: String,
    guest_token: String,
}

impl Guest {
    async fn new(bearer: &str) -> Result<Guest> {
        let start = Instant::now();
        let client = reqwest::Client::new();
        let bearer_token = format!("Bearer {bearer}");

        let guest_token = client
            .post("https://api.twitter.com/1.1/guest/activate.json")
            .header(AUTHORIZATION, &bearer_token)
            .send()
            .await
            .with_context(|| "Error fetching guest token".to_string())?
            .json::<serde_json::Value>()
            .await
            .with_context(|| "Guest token response was not json".to_string())?
            .get("guest_token")
            .and_then(|f| f.as_str())
            .ok_or_else(|| anyhow!("No guest_token attribute found"))?
            .to_string();

        println!(
            "Guest fetched in {}ms:\n{guest_token}",
            start.elapsed().as_millis()
        );
        Ok(Self {
            bearer_token,
            guest_token,
        })
    }

    async fn space<'a>(&'a self, id: &str) -> Result<Space<'a>> {
        Space::new(self, id).await
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response> {
        Ok(reqwest::Client::new()
            .get(url)
            .header(AUTHORIZATION, &self.bearer_token)
            .header("X-Guest-Token", &self.guest_token)
            .send()
            .await?)
    }
}

struct Space<'a> {
    guest: &'a Guest,
    attrs: SpaceAttrs,
    name: String,
    admins: String,
}

impl<'a> Space<'a> {
    async fn new(guest: &'a Guest, id: &str) -> Result<Space<'a>> {
        let start = Instant::now();
        let id = id.split('?').collect::<Vec<&str>>()[0]
            .replace("https://", "")
            .replace("twitter.com/i/spaces/", "")
            .replace("/", "");

        let address = format!(
            "https://twitter.com/i/api/graphql/Uv5R_-Chxbn1FEkyUkSW2w/AudioSpaceById?variables=%7B%22id%22%3A%22{id}%22%2C%22isMetatagsQuery%22%3Afalse%2C%22withBirdwatchPivots%22%3Afalse%2C%22withDownvotePerspective%22%3Afalse%2C%22withReactionsMetadata%22%3Afalse%2C%22withReactionsPerspective%22%3Afalse%2C%22withReplays%22%3Afalse%2C%22withScheduledSpaces%22%3Afalse%2C%22withSuperFollowsTweetFields%22%3Afalse%2C%22withSuperFollowsUserFields%22%3Afalse%7D",
        );

        let res = guest.get(&address).await?;

        let attrs = res.json::<SpaceAttrs>().await?;

        let name = attrs
            .data
            .audio_space
            .metadata
            .title
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace() || "—-_".contains(&c.to_string()))
            .collect();

        let admins = attrs
            .data
            .audio_space
            .participants
            .admins
            .iter()
            .map(|admin| format!("{}{}", admin.display_name, ","))
            .collect();

        println!("Space info fetched in {}ms", start.elapsed().as_millis());

        Ok(Self {
            guest,
            attrs,
            name,
            admins,
        })
    }

    async fn download(&self, name: Option<String>, concurrency: usize) -> Result<()> {
        let stream = self.stream().await?;
        let start = Instant::now();
        println!(
            "Admins: {}\nTitle: {}\nLocation: {}",
            self.admins,
            self.name,
            stream.location()
        );

        let final_file = format!("{}.aac", name.as_ref().unwrap_or(&self.name));
        File::create(&final_file).await?;
        let file = OpenOptions::new().append(true).open(&final_file).await?;
        stream.download_fragments(concurrency, file).await?;

        println!("\nSpace downloaded in {}ms", start.elapsed().as_millis());

        Ok(())
    }

    async fn stream(&'a self) -> Result<Stream<'a>> {
        Stream::new(self).await
    }
}

struct Stream<'a> {
    fragment_dir: String,
    space: &'a Space<'a>,
    attrs: StreamAttrs,
}

impl<'a> Stream<'a> {
    pub async fn new(space: &'a Space<'a>) -> Result<Stream<'a>> {
        let start = Instant::now();
        let address = format!(
            "https://twitter.com/i/api/1.1/live_video_stream/status/{}",
            &space.attrs.data.audio_space.metadata.media_key
        );
        let attrs = space
            .guest
            .get(&address)
            .await?
            .json::<StreamAttrs>()
            .await?;

        let fragment_dir = temp_dir()
            .to_str()
            .ok_or_else(|| anyhow!("Could not get temporary directory"))?
            .to_string()
            + "/moonbird";

        if !Path::new(&fragment_dir).exists() {
            create_dir(&fragment_dir)?;
        }

        println!("Stream fetched in {}ms", start.elapsed().as_millis());

        Ok(Self {
            attrs,
            space,
            fragment_dir,
        })
    }

    pub async fn download_fragments(&self, concurrency: usize, final_file: File) -> Result<()> {
        let base_uri = self
            .location()
            .split("playlist")
            .next()
            .ok_or_else(|| anyhow!("Could not parse base_uri from location"))?;

        let client = &reqwest::Client::new();
        let fragments = &self.fragments().await?;
        let final_file = Arc::new(Mutex::new(final_file));

        let size = fragments.len();
        let policy = &RetryPolicy::exponential(Duration::from_secs(1))
            .with_max_retries(5)
            .with_jitter(true);
        let notifications = &(0..size).map(|_| Notify::new()).collect::<Vec<Notify>>();

        println!("Fragments: {size}");

        let futures = fragments
            .into_iter()
            .enumerate()
            .map(|(index, fragment_name)| {
                let url = format!("{base_uri}{fragment_name}");
                let final_file = final_file.clone();
                async move {
                    let bytes = policy
                        .retry(|| client.get(&url).send())
                        .await
                        .expect(&format!("Error while downloading fragment #{index}"))
                        .bytes()
                        .await
                        .expect(&format!(
                            "Error while extracting bytes for fragment #{index}"
                        ));

                    (bytes.to_vec(), index, final_file)
                }
            });

        iter(futures)
            .buffer_unordered(concurrency)
            .for_each_concurrent(None, |(bytes, index, final_file)| async move {
                if index != 0 {
                    notifications[index - 1].notified().await
                }
                final_file
                    .lock()
                    .await
                    .write_all(bytes.as_slice())
                    .await
                    .expect(&format!("Error writing fragment #{index}"));
                print!(
                    " fragments remaining \r{}",
                    size - index - 1
                );
                notifications[index].notify_one();
            })
            .await;

        remove_dir_all(&self.fragment_dir)?;

        Ok(())
    }

    async fn fragments(&self) -> Result<Vec<String>> {
        Ok(self
            .space
            .guest
            .get(self.location())
            .await?
            .text()
            .await?
            .split('\n')
            .filter(|c| !c.contains('#'))
            .map(str::to_string)
            .collect())
    }

    pub fn location(&'a self) -> &'a str {
        &self.attrs.source.location
    }
}

#[derive(Debug, Deserialize)]
struct SpaceAttrs {
    data: Data,
}

#[derive(Debug, Serialize, Deserialize)]
struct Data {
    #[serde(rename(serialize = "audioSpace", deserialize = "audioSpace"))]
    audio_space: AudioSpace,
}

#[derive(Debug, Serialize, Deserialize)]
struct AudioSpace {
    metadata: Metadata,
    participants: Participants,
}

#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    media_key: String,
    title: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Participants {
    admins: Vec<Admin>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Admin {
    display_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StreamAttrs {
    source: Source,
}

#[derive(Debug, Serialize, Deserialize)]
struct Source {
    location: String,
}
