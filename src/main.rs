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
use url::{Host, ParseError, Url};

type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;
type DynResult<T> = std::result::Result<T, DynError>;
//type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = DynResult<T>> + Send>>;

mod util;

// constants
const GET_REQUEST_TIMEOUT: Duration = Duration::from_millis(20000);
const MAX_RECURSION_DEPTH: u8 = 4;
const MAX_HOST_VISITORS: u32 = 512;
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:76.0) Gecko/20100101 Firefox/76.0";

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
    archive: HashSet<Finding>,
    host_visits: HashMap<Host, u32>,

    crawlers: FuturesUnordered<JoinHandle<reqwest::Result<CrawlResponse>>>,
    image_fetchers: FuturesUnordered<JoinHandle<DynResult<()>>>,
}

// page can be crawled
// resource can be fetched
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Finding {
    Page(Url, u8),
    Image(Url),
}

struct CrawlResponse {
    new_findings: HashSet<Finding>,
    crawl_link: Url,
    depth: u8,
}

impl Dispatcher {
    fn new(
        inital_urls: HashSet<Url>,
        max_recursion_depth: u8,
    ) -> Result<Dispatcher, reqwest::Error> {
        let client = Client::builder()
            .connect_timeout(GET_REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        Ok(Dispatcher {
            client,
            inital_urls,
            max_recursion_depth,
            archive: HashSet::new(),
            host_visits: HashMap::new(),
            crawlers: FuturesUnordered::new(),
            image_fetchers: FuturesUnordered::new(),
        })
    }

    async fn run(&mut self) {
        let mut queue: Vec<Finding> = self
            .inital_urls
            .iter()
            .cloned()
            .map(|u| Finding::Page(u, 0))
            .collect();
        let (mut crawlers_finished, mut fetchers_finished) = (false, false);

        while !(crawlers_finished && fetchers_finished) {
            queue.drain_filter(|f| {
                let url = match &f {
                    Finding::Page(url, _) => url,
                    Finding::Image(url) => url,
                };
                let host = url.host().expect("url must have host").to_owned();
                let visits = self.host_visits.entry(host.clone()).or_insert(0);

                if *visits < MAX_HOST_VISITORS {
                    *visits += 1;
                    match f {
                        Finding::Page(_, ref depth) => self.crawlers.push(task::spawn(crawl_page(
                            url.clone(),
                            self.client.clone(),
                            *depth,
                        ))),
                        Finding::Image(..) => self
                            .image_fetchers
                            .push(task::spawn(fetch_image(url.clone(), self.client.clone()))),
                    };
                    true
                } else {
                    debug!(
                        "too many visitors on `{}`, requeueing `{}`",
                        host.to_string(),
                        url
                    );
                    false
                }
            });

            // Destructuring assignment not available
            crawlers_finished = false;
            fetchers_finished = false;

            match self.crawlers.next().await {
                None => crawlers_finished = true,
                Some(Err(e)) => warn!("{}", e),
                Some(Ok(Err(e))) => warn!("{}", e),
                Some(Ok(Ok(crawl_response))) => {
                    let CrawlResponse {
                        mut new_findings,
                        crawl_link,
                        depth,
                    } = crawl_response;

                    let host = crawl_link.host().expect("url must have host").to_owned();
                    self.host_visits.entry(host).and_modify(|n| *n -= 1);

                    new_findings = new_findings.difference(&self.archive).cloned().collect();
                    self.archive.extend(new_findings.clone());

                    // queue findings depending on depth
                    if depth < self.max_recursion_depth {
                        queue.extend(new_findings);
                    }
                }
            }

            match self.image_fetchers.next().await {
                None => fetchers_finished = true,
                Some(Err(e)) => warn!("{}", e),
                Some(Ok(Err(e))) => warn!("{}", e),
                Some(Ok(Ok(_))) => {}
            }
        }
    }
}

async fn crawl_page(crawl_link: Url, client: Client, depth: u8) -> reqwest::Result<CrawlResponse> {
    info!("crawling url `{}`", &crawl_link);

    let request = client.get(crawl_link.clone());
    let response = request.send().await?;
    let body = response.text().await?;

    let new_findings = process_page(&crawl_link, body, depth);
    Ok(CrawlResponse {
        new_findings,
        crawl_link,
        depth,
    })
}

fn process_page(page_url: &Url, page_body: String, depth: u8) -> HashSet<Finding> {
    let mut page_url = page_url.clone();
    page_url.set_path("");
    page_url.set_query(None);

    let mut raw_findings = Aggregate::new(depth);
    let mut tokenizer = Tokenizer::new(&mut raw_findings, TokenizerOpts::default());
    let mut buffer = BufferQueue::new();
    buffer.push_back(page_body.into());
    let _ = tokenizer.feed(&mut buffer);

    raw_findings.parse(&page_url)
}

#[derive(Debug)]
struct Aggregate {
    depth: u8,
    page_links: Vec<String>,
    image_links: Vec<String>,
}

impl Aggregate {
    fn new(depth: u8) -> Self {
        Aggregate {
            depth,
            page_links: Vec::new(),
            image_links: Vec::new(),
        }
    }
}

impl Aggregate {
    fn parse(self, page_url: &Url) -> HashSet<Finding> {
        let mut findings = HashSet::new();

        let page_links = parse_links(self.page_links, page_url);
        let image_links = parse_links(self.image_links, page_url);
        let depth = self.depth;

        findings.extend(page_links.into_iter().map(|u| Finding::Page(u, depth)));
        findings.extend(image_links.into_iter().map(Finding::Image));

        findings
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

impl TokenSink for &mut Aggregate {
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
                            let url_string = String::from_utf8_lossy(url_str).into_owned();
                            self.page_links.push(url_string);
                        }
                    }
                }
                "img" => {
                    for attribute in tag.attrs.iter() {
                        if attribute.name.local.as_ref() == "src" {
                            let url_str: &[u8] = attribute.value.borrow();
                            let url_string = String::from_utf8_lossy(url_str).into_owned();
                            self.image_links.push(url_string);
                        }
                    }
                }
                _ => {}
            }
        }
        TokenSinkResult::Continue
    }
}

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
