//!
#![warn(missing_debug_implementations, rust_2018_idioms)]
#![feature(drain_filter)]

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    time::Duration,
};

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use futures::{prelude::*, stream::FuturesUnordered};

use tokio::{
    fs::File,
    prelude::*,
    task::{self, JoinHandle},
};

use reqwest::Client;

use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use url::{ParseError, Url};

type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;
type DynResult<T> = std::result::Result<T, DynError>;
//type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = DynResult<T>> + Send>>;

mod util;

// constants
const GET_REQUEST_TIMEOUT: Duration = Duration::from_millis(20000);
const MAX_RECURSION_DEPTH: u8 = 4;
const MAX_HOST_VISITORS: u32 = 512;

#[tokio::main]
async fn main() -> DynResult<()> {
    setup_logger()?;
    let AppInput {
        inital_urls,
        max_recursion_depth,
    } = setup_app();
    info!("crawling these urls:\n{:?}", &inital_urls);
    let mut dispatcher = Dispatcher::new(inital_urls, max_recursion_depth)?;
    dispatcher.run().await;

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
        .level(log::LevelFilter::Info)
        .level_for("surf::middleware::logger::native", log::LevelFilter::Error)
        .chain(term_dispatch)
        .chain(file_dispatch)
        .apply()?;

    Ok(())
}

struct AppInput {
    inital_urls: HashSet<Url>,
    max_recursion_depth: u8,
}

fn setup_app() -> AppInput {
    use clap::Arg;

    let app = clap::App::new("crawler")
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

    let matches = app.get_matches();
    let max_recursion_depth = matches
        .value_of("depth")
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_RECURSION_DEPTH);
    let inital_urls = matches
        .values_of("url")
        .unwrap()
        .map(|url| match Url::parse(url) {
            Ok(url) => url,
            Err(err) => panic!("{}", err),
        })
        .collect::<HashSet<Url>>();

    AppInput {
        inital_urls,
        max_recursion_depth,
    }
}

// schedules and dispatches crawlers on pages
// keep track of urls already crawled to avoid recrawling
// keep track of amount of crawlers on _domain_ to avoid overloading page
#[derive(Debug)]
struct Dispatcher {
    client: Client,
    inital_urls: HashSet<Url>,
    max_recursion_depth: u8,
    archive: Vec<Finding>,
    host_visits: HashMap<url::Host, u32>,
    crawlers: FuturesUnordered<JoinHandle<reqwest::Result<CrawlResponse>>>,
    image_fetchers: FuturesUnordered<JoinHandle<DynResult<()>>>,
}

struct Queue {
    page_links: Vec<Url>,
    image_links: Vec<Url>,
}

#[derive(Default, Debug)]
struct RawFinding {
    page_links: Vec<String>,
    image_links: Vec<String>,
    depth: u8,
}

#[derive(Default, Debug, Clone)]
struct Finding {
    page_links: HashSet<Url>,
    image_links: HashSet<Url>,
    depth: u8,
}

impl Dispatcher {
    fn new(
        inital_urls: HashSet<Url>,
        max_recursion_depth: u8,
    ) -> Result<Dispatcher, reqwest::Error> {
        let client = Client::builder()
            .connect_timeout(GET_REQUEST_TIMEOUT)
            .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:76.0) Gecko/20100101 Firefox/76.0")
            .build()?;

        Ok(Dispatcher {
            client,
            inital_urls,
            max_recursion_depth,
            archive: Vec::new(),
            host_visits: HashMap::new(),
            crawlers: FuturesUnordered::new(),
            image_fetchers: FuturesUnordered::new(),
        })
    }

    async fn run(&mut self) {
        let mut queue = Queue {
            page_links: self.inital_urls.iter().cloned().collect(),
            image_links: Vec::new(),
        };

        loop {
            queue.page_links.drain_filter(|l| {
                let host = l.host().expect("url must have host").to_owned();
                let visits = self.host_visits.entry(host.clone()).or_insert(0);

                if *visits < MAX_HOST_VISITORS {
                    *visits += 1;
                    self.crawlers
                        .push(task::spawn(crawl_page(l.clone(), self.client.clone())));
                    true
                } else {
                    info!(
                        "too many visitors on `{}`, requeueing `{}`",
                        host.to_string(),
                        l
                    );
                    false
                }
            });

            queue.image_links.drain_filter(|l| {
                let host = l.host().expect("url must have host").to_owned();
                let visits = self.host_visits.entry(host.clone()).or_insert(0);

                if *visits < MAX_HOST_VISITORS {
                    *visits += 1;
                    self.image_fetchers
                        .push(task::spawn(fetch_image(l.clone(), self.client.clone())));
                    true
                } else {
                    info!(
                        "too many visitors on `{}`, requeueing `{}`",
                        host.to_string(),
                        l
                    );
                    false
                }
            });

            let mut crawler_finished = false;
            let mut image_fetchers_finished = false;

            match self.crawlers.next().await {
                None => crawler_finished = true,
                Some(Err(e)) => warn!("{}", e),
                Some(Ok(Err(e))) => warn!("{}", e),
                Some(Ok(Ok(crawl_response))) => {
                    let CrawlResponse {
                        mut new_finding,
                        page_link,
                    } = crawl_response;

                    let host = page_link.host().expect("url must have host").to_owned();
                    self.host_visits.entry(host).and_modify(|n| *n -= 1);

                    for finding in &self.archive {
                        new_finding = new_finding.difference(finding);
                    }
                    self.archive.push(new_finding.clone());

                    // queue findings depending on depth
                    if new_finding.depth < self.max_recursion_depth {
                        queue.page_links.extend(new_finding.page_links);
                        queue.image_links.extend(new_finding.image_links);
                    }
                }
            }

            match self.image_fetchers.next().await {
                None => image_fetchers_finished = true,
                Some(Err(e)) => warn!("{}", e),
                Some(Ok(Err(e))) => warn!("{}", e),
                Some(Ok(Ok(_))) => {}
            }

            if crawler_finished && image_fetchers_finished {
                break;
            }
        }
    }
}

impl RawFinding {
    fn parse(self, page_url: &Url) -> Finding {
        let page_links = parse_links(self.page_links, page_url);
        let image_links = parse_links(self.image_links, page_url);
        Finding {
            page_links,
            image_links,
            depth: self.depth,
        }
    }
}

impl Finding {
    fn difference(&self, other: &Self) -> Finding {
        Finding {
            page_links: self
                .page_links
                .difference(&other.page_links)
                .cloned()
                .collect(),
            image_links: self
                .image_links
                .difference(&other.image_links)
                .cloned()
                .collect(),
            depth: self.depth,
        }
    }
}

fn parse_links(links: Vec<String>, page_url: &Url) -> HashSet<Url> {
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
        .filter(|u| u.scheme().contains("http"))
        .filter(|u| u.host().is_some())
        .collect()
}

// refactor?
struct CrawlResponse {
    new_finding: Finding,
    page_link: Url,
}

async fn crawl_page(page_link: Url, client: Client) -> reqwest::Result<CrawlResponse> {
    info!("crawling url `{}`", &page_link);

    let request = client.get(page_link.clone());
    let response = request.send().await?;
    let body = response.text().await?;

    let new_finding = process_page(&page_link, body);
    Ok(CrawlResponse {
        new_finding,
        page_link,
    })
}

fn process_page(page_url: &Url, page_body: String) -> Finding {
    let mut page_url = page_url.clone();
    page_url.set_path("");
    page_url.set_query(None);

    let mut raw_findings = RawFinding::default();
    let mut tokenizer = Tokenizer::new(&mut raw_findings, TokenizerOpts::default());
    let mut buffer = BufferQueue::new();
    buffer.push_back(page_body.into());
    let _ = tokenizer.feed(&mut buffer);

    raw_findings.parse(&page_url)
}

impl TokenSink for &mut RawFinding {
    type Handle = ();

    fn process_token(&mut self, token: Token, _line_number: u64) -> TokenSinkResult<Self::Handle> {
        if let TagToken(
            ref
            tag @ Tag {
                kind: TagKind::StartTag,
                ..
            },
        ) = token
        {
            match tag.name.as_ref() {
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
            }
        }
        TokenSinkResult::Continue
    }
}

#[allow(dead_code)]
async fn fetch_image(url: Url, client: Client) -> DynResult<()> {
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

    let request = client.get(url.clone());
    let response = request.send().await?;
    let bytes = response.bytes().await?;

    let _ = file.write_all(&bytes).await?;
    Ok(())
}

#[allow(dead_code)]
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
