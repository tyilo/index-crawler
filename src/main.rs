#![deny(clippy::pedantic)]

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use clap::Parser;
use color_eyre::Result;
use futures_lite::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressFinish, ProgressStyle};
use mime::{Mime, TEXT_HTML};
use reqwest::{Url, header::CONTENT_TYPE};
use reqwest_extra::ResponseExt;
use scraper::{Html, Selector};
use tokio::{io::AsyncWriteExt, sync::mpsc, task::JoinHandle};

struct RunOnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for RunOnDrop<F> {
    fn drop(&mut self) {
        self.0();
    }
}

struct Crawler {
    client: reqwest::Client,
    start_url: Url,
    out_dir: PathBuf,
    no_save_files: bool,
    link_selector: Selector,
    visited_paths: Mutex<HashSet<PathBuf>>,
    multi_progress: MultiProgress,
    main_progress: ProgressBar,
}

type TX = mpsc::UnboundedSender<JoinHandle<()>>;

impl Crawler {
    fn new(start_url: Url, out_dir: PathBuf, no_save_files: bool) -> Result<Self> {
        let multi_progress = MultiProgress::new();
        let main_progress = multi_progress.add(
            ProgressBar::new(0)
                .with_style(
                    ProgressStyle::with_template("{wide_bar} {msg} {human_pos}/{human_len}")
                        .unwrap(),
                )
                .with_message("Pages downloaded:")
                .with_finish(ProgressFinish::AndLeave),
        );
        Ok(Self {
            client: reqwest::Client::builder()
                .tcp_user_timeout(Duration::from_secs(1))
                .build()?,
            start_url,
            out_dir,
            no_save_files,
            link_selector: Selector::parse("a[href]").unwrap(),
            visited_paths: Mutex::new(HashSet::new()),
            multi_progress,
            main_progress,
        })
    }

    async fn crawl(self: Arc<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        self.crawl_url(&self.start_url, &tx).await;
        drop(tx);

        while let Some(handle) = rx.recv().await {
            if let Err(e) = handle.await {
                self.main_progress
                    .println(format!("Failed to join task: {e}"));
            }
        }
    }

    fn path(&self, url: &Url) -> Option<PathBuf> {
        let mut url = url.clone();
        url.set_fragment(None);
        url.set_query(None);
        let relative = self.start_url.make_relative(&url)?;
        if relative.starts_with("..") {
            return None;
        }
        Some(PathBuf::from(relative))
    }

    // Using `async fn` doesn't work as Rust can't figure out that the future is
    // actually `Send`
    #[allow(clippy::manual_async_fn)]
    fn crawl_url(self: &Arc<Self>, url: &Url, tx: &TX) -> impl Future<Output = ()> + Send {
        async move {
            if let Err(e) = self.crawl_url_inner(url, tx).await {
                self.main_progress
                    .println(format!("Failed to crawl {url}: {e:?}"));
            }
        }
    }

    async fn crawl_url_inner(self: &Arc<Self>, url: &Url, tx: &TX) -> Result<()> {
        let Some(path) = self.path(url) else {
            self.main_progress.println(format!("Skipping {url}"));
            return Ok(());
        };

        if !self.visited_paths.lock().unwrap().insert(path.clone()) {
            return Ok(());
        }

        self.main_progress.inc_length(1);
        let _defer = RunOnDrop(|| {
            self.main_progress.inc(1);
        });

        self.main_progress.println(format!("Downloading {url}"));

        let response = self
            .client
            .get(url.clone())
            .send()
            .await?
            .error_for_status_with_body()
            .await?;

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<Mime>().ok());
        let is_html = content_type.is_some_and(|v| v.essence_str() == TEXT_HTML);

        if !is_html {
            let pb = self.multi_progress.insert_before(
                &self.main_progress,
                ProgressBar::no_length()
                    .with_style(
                        ProgressStyle::with_template(
                            "{msg:40!} {bar:20} {binary_bytes}/{binary_total_bytes} \
                             {elapsed_precise}/{duration_precise}",
                        )
                        .unwrap(),
                    )
                    .with_finish(ProgressFinish::AndLeave)
                    .with_message(url.to_string()),
            );
            if let Some(content_length) = response.content_length() {
                pb.set_length(content_length);
            }

            let file_path = self.out_dir.join(path);

            if !self.no_save_files {
                let mut stream = response.bytes_stream();
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let mut file = tokio::fs::File::create_new(&file_path).await?;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    file.write_all(&chunk).await?;
                    pb.inc(chunk.len().try_into().unwrap());
                }
            }
            self.main_progress
                .println(format!("Saved file {}", file_path.display()));
            return Ok(());
        }

        let html = response.text().await?;
        let html = Html::parse_document(&html);

        for element in html.select(&self.link_selector) {
            let Some(href) = element.attr("href") else {
                continue;
            };
            match url.join(href) {
                Ok(url) => {
                    let this = self.clone();
                    tx.send({
                        let tx = tx.clone();
                        tokio::task::spawn(async move { this.crawl_url(&url, &tx).await })
                    })?;
                }
                Err(e) => self.main_progress.println(format!(
                    "Failed to parse href value {href:?} on page {url}: {e}"
                )),
            }
        }

        Ok(())
    }
}

#[derive(Parser)]
struct Cli {
    url: Url,
    #[clap(long)]
    out_dir: Option<PathBuf>,
    #[clap(long)]
    no_save_files: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let args = Cli::parse();
    // See https://github.com/clap-rs/clap/issues/4746
    let out_dir = args.out_dir.unwrap_or_default();

    let crawler = Arc::new(Crawler::new(args.url.clone(), out_dir, args.no_save_files)?);
    crawler.crawl().await;

    Ok(())
}
