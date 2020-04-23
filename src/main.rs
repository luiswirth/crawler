//!
#![warn(missing_debug_implementations, rust_2018_idioms)]

use chrono;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use clap::{App, Arg};

use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use url::{ParseError, Url};

use surf;

use async_std::{
    fs::File,
    future::timeout,
    io::prelude::*,
    sync::{Arc, Mutex},
    task,
};

use std::borrow::Borrow;
use std::time::Duration;

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
type Result<T> = std::result::Result<T, Error>;
type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>>;

// goal: use _shared-state concurrency_ (multiple ownership) to accumulate the findings
// from all tasks into one collection.

const GET_REQUEST_TIMEOUT: Duration = Duration::from_millis(4000);

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

impl UnparsedFindings {
    fn parse(self, page_url: &Url) -> Findings {
        Findings {
            page_links: parse_links(self.page_links, page_url),
            image_links: parse_links(self.image_links, page_url),
        }
    }
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

async fn crawl_loop(
    pages: Vec<Url>,
    current: u8,
    max: u8,
    findings: Arc<Mutex<Findings>>,
) -> Result<()> {
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
            Ok::<(), Error>(())
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

// wraper function for pinning
fn box_crawl_loop(
    pages: Vec<Url>,
    current: u8,
    max: u8,
    findings: Arc<Mutex<Findings>>,
) -> BoxFuture<()> {
    Box::pin(crawl_loop(pages, current, max, findings))
}

async fn fetch_images(urls: Vec<Url>) -> Result<()> {
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
            Ok::<(), Error>(())
        });
        tasks.push(task);
    }
    for task in tasks.into_iter() {
        task.await?;
    }
    Ok(())
}

pub fn crawl(pages: Vec<Url>, max_recursion: u8) -> Result<()> {
    let findings = Arc::new(Mutex::new(Findings::default()));
    task::block_on(async {
        box_crawl_loop(pages, 0, max_recursion, Arc::clone(&findings)).await?;
        let findings = Arc::try_unwrap(findings).unwrap(); // remove Arc hull
        let findings_inner = findings.into_inner(); // remove Mutex hull
        fetch_images(findings_inner.image_links).await?;
        Ok::<(), Error>(())
    })?;
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

fn main() -> Result<()> {
    setup_logger()?;

    let app = App::new("crawler")
        .version(env!("CARGO_PKG_NAME"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .usage("crawler [OPTIONS] <url1> [url2] ...")
        .before_help("")
        .after_help("")
        .arg(
            Arg::with_name("url")
                .value_name("URL")
                .help("URL to start crawling from")
                .takes_value(true)
                .required(true)
                .multiple(true),
        )
        .arg(
            Arg::with_name("depth")
                .short("d")
                .long("depth")
                .value_name("int")
                .help("max recursion depth")
                .takes_value(true)
                .default_value("2"),
        );

    let matches = app.get_matches();
    let depth = matches.value_of("depth").unwrap().parse().unwrap();
    let urls = matches
        .values_of("url")
        .unwrap()
        .map(|url| match Url::parse(url) {
            Ok(url) => url,
            Err(err) => panic!("{}", err),
        })
        .collect::<Vec<Url>>();

    info!("crawling these urls:\n{:?}", urls);

    crawl(urls, depth)?;
    Ok(())
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
