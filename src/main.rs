//!
#![warn(missing_debug_implementations, rust_2018_idioms)]

use chrono;
use std::borrow::Borrow;
use std::time::Duration;

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use async_std::{
    fs::File,
    future::timeout,
    prelude::*,
    sync::{Arc, Mutex},
    task,
};
use futures::{channel::mpsc, sink::SinkExt};

use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use surf;
use url::{ParseError, Url};

type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;
type DynResult<T> = std::result::Result<T, DynError>;
type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = DynResult<T>> + Send>>;

type Sender<T> = mpsc::UnboundedSender<T>;
type Receiver<T> = mpsc::UnboundedReceiver<T>;

// constants
const GET_REQUEST_TIMEOUT: Duration = Duration::from_millis(4000);

fn main() -> DynResult<()> {
    setup_logger()?;
    let crawler = setup_app();

    info!("crawling these urls:\n{:?}", crawler.inital_urls);

    crawl(crawler.inital_urls, crawler.max_recursion_depth)?;
    Ok(())
}

fn setup_logger() -> std::result::Result<(), fern::InitError> {
    use fern::{
        colors::{Color, ColoredLevelConfig},
        Dispatch,
    };

    let file_dispatch = Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%H:%M:%S]"),
                record.target(),
                record.level(),
                message
            ))
        })
        .chain(fern::log_file(format!(
            "logs/{}.log",
            chrono::Local::now().format("%Y-%m-%d_%H%M%S")
        ))?);

    let colors = ColoredLevelConfig::new()
        .trace(Color::Blue)
        .info(Color::Green)
        .warn(Color::Magenta)
        .error(Color::Red);

    let term_dispatch = Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%H:%M:%S]"),
                record.target(),
                colors.color(record.level()),
                message
            ))
        })
        .chain(std::io::stdout());

    Dispatch::new()
        .level(log::LevelFilter::Warn)
        .level_for("surf::middleware::logger::native", log::LevelFilter::Error)
        .chain(term_dispatch)
        .chain(file_dispatch)
        .apply()?;

    Ok(())
}

#[derive(Default, Debug)]
struct Crawler {
    inital_urls: Vec<Url>,
    max_recursion_depth: u8,
    findings: Findings,
}

fn setup_app() -> Crawler {
    use clap::{App as ClapApp, Arg};

    let app = ClapApp::new("crawler")
        .version(env!("CARGO_PKG_NAME"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .usage("crawler [OPTIONS] <url1> [url2 ...]")
        .before_help("")
        .after_help("")
        .arg(
            Arg::with_name("url")
                .value_name("url")
                .help("URL to start crawling from")
                .takes_value(true)
                .required(true)
                .multiple(true),
        )
        .arg(
            Arg::with_name("depth")
                .short("d")
                .long("depth")
                .value_name("depth")
                .help("max recursion depth")
                .takes_value(true)
                .default_value("2"),
        );

    let mut crawler = Crawler::default();

    let matches = app.get_matches();
    crawler.max_recursion_depth = matches.value_of("depth").unwrap().parse().unwrap();
    crawler.inital_urls = matches
        .values_of("url")
        .unwrap()
        .map(|url| match Url::parse(url) {
            Ok(url) => url,
            Err(err) => panic!("{}", err),
        })
        .collect::<Vec<Url>>();

    crawler
}

pub fn crawl(pages: Vec<Url>, max_recursion: u8) -> DynResult<()> {
    let findings = Arc::new(Mutex::new(Findings::default()));
    task::block_on(async {
        box_crawl_loop(pages, 0, max_recursion, Arc::clone(&findings)).await?;
        let findings = Arc::try_unwrap(findings).unwrap(); // remove Arc hull
        let findings_inner = findings.into_inner(); // remove Mutex hull
        fetch_images(findings_inner.image_links).await?;
        Ok::<(), DynError>(())
    })?;
    Ok(())
}

// wraper function for pinning
fn box_crawl_loop(
    pages: Vec<Url>,
    current: u8,
    max: u8,
    findings: Arc<Mutex<Findings>>,
) -> BoxFuture<()> {
    Box::pin(crawl_loop(pages, current, max, findings))
}

async fn crawl_loop(
    pages: Vec<Url>,
    current: u8,
    max: u8,
    findings: Arc<Mutex<Findings>>,
) -> DynResult<()> {
    debug!("Current recursion depth: {}, max depth: {}", current, max);

    if current >= max {
        return Ok(());
    }

    let mut tasks = Vec::new();
    for url in pages {
        let findings = Arc::clone(&findings);
        let task = task::spawn(async move {
            info!("crawling url `{}`", &url);

            let future = timeout(GET_REQUEST_TIMEOUT, surf::get(&url).recv_string());
            let body = match future.await {
                Err(_) => {
                    warn!("GET-request timeout with url `{}`", &url);
                    return Ok(());
                }
                Ok(request) => match request {
                    Ok(page) => page,
                    Err(err) => {
                        error!("GET-request error `{}` with url `{}`", err, &url);
                        return Ok(());
                    }
                },
            };
            let new_findings = process_page(&url, body);

            {
                let mut findings_inner = findings.lock().await;
                // @TODO: more idomatic way of extending all
                findings_inner
                    .page_links
                    .extend_from_slice(&new_findings.page_links);
                findings_inner
                    .image_links
                    .extend_from_slice(&new_findings.image_links);
            }

            let new_page_links = new_findings.page_links;
            box_crawl_loop(new_page_links, current + 1, max, findings).await?;
            Ok::<(), DynError>(())
        });
        debug!("new task spawned");
        tasks.push(task);
    }

    for task in tasks.into_iter() {
        task.await?;
        debug!("task done");
    }

    Ok(())
}

#[derive(Default, Debug)]
struct UnparsedFindings {
    page_links: Vec<String>,
    image_links: Vec<String>,
}

#[derive(Default, Debug)]
struct Findings {
    page_links: Vec<Url>,
    image_links: Vec<Url>,
}

impl UnparsedFindings {
    fn parse(self, page_url: &Url) -> Findings {
        Findings {
            page_links: parse_links(self.page_links, page_url),
            image_links: parse_links(self.image_links, page_url),
        }
    }
}

fn parse_links(links: Vec<String>, page_url: &Url) -> Vec<Url> {
    links
        .into_iter()
        .filter_map(|l| match Url::parse(&l) {
            Err(ParseError::RelativeUrlWithoutBase) => Some(page_url.join(&l).unwrap()),
            Err(_) => {
                warn!("Malformed link found: {}", l);
                None
            }
            Ok(url) => Some(url),
        })
        .collect()
}

fn process_page(page_url: &Url, page_body: String) -> Findings {
    let mut page_url = page_url.clone();
    page_url.set_path("");
    page_url.set_query(None);

    let mut findings = UnparsedFindings::default();
    let mut tokenizer = Tokenizer::new(&mut findings, TokenizerOpts::default());
    let mut buffer = BufferQueue::new();
    buffer.push_back(page_body.into());
    let _ = tokenizer.feed(&mut buffer);

    findings.parse(&page_url)
}

impl TokenSink for &mut UnparsedFindings {
    type Handle = ();

    fn process_token(&mut self, token: Token, _line_number: u64) -> TokenSinkResult<Self::Handle> {
        match token {
            TagToken(
                ref
                tag
                @
                Tag {
                    kind: TagKind::StartTag,
                    ..
                },
            ) => match tag.name.as_ref() {
                "a" => {
                    for attribute in tag.attrs.iter() {
                        if attribute.name.local.as_ref() == "href" {
                            let url_str: &[u8] = attribute.value.borrow();
                            self.page_links
                                .push(String::from_utf8_lossy(url_str).into_owned());
                        }
                    }
                }
                "img" => {
                    for attribute in tag.attrs.iter() {
                        if attribute.name.local.as_ref() == "src" {
                            let url_str: &[u8] = attribute.value.borrow();
                            self.image_links
                                .push(String::from_utf8_lossy(url_str).into_owned());
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

async fn fetch_images(urls: Vec<Url>) -> DynResult<()> {
    let mut tasks = Vec::new();
    for url in urls.into_iter() {
        let task = task::spawn(async move {
            info!("fetching `{}`", url);
            let path_segments = match url.path_segments() {
                Some(segs) => segs,
                None => {
                    return Ok(());
                }
            };
            let file_path = path_segments.last().unwrap();
            let file_path = format!("archive/imgs/{}", file_path);
            let mut file = File::create(file_path).await?;

            let request = timeout(GET_REQUEST_TIMEOUT, surf::get(&url).recv_bytes());
            let request = match request.await {
                Ok(request) => request,
                Err(_) => {
                    warn!("GET-request timeout with url `{}`", &url);
                    return Ok(());
                }
            };

            let img_bytes = match request {
                Ok(res) => res,
                Err(err) => {
                    error!("GET-request error `{}` with url `{}`", err, &url);
                    return Ok(());
                }
            };
            let _ = file.write(&img_bytes).await?;
            Ok::<(), DynError>(())
        });
        tasks.push(task);
    }
    for task in tasks.into_iter() {
        task.await?;
    }
    Ok(())
}

fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
where
    F: Future<Output = DynResult<()>> + Send + 'static,
{
    task::spawn(async move {
        if let Err(e) = fut.await {
            error!("{}", e);
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_suite_works_0() {
        assert!(true);
    }

    #[test]
    fn test_suite_works_1() -> Result<(), ()> {
        Ok(())
    }

    #[test]
    #[should_panic]
    fn test_suite_works_2() {
        panic!()
    }
}
