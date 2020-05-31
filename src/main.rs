//!
#![warn(
    rust_2018_idioms,
    missing_debug_implementations,
    deprecated_in_future,
    confusable_idents,
    elided_lifetimes_in_paths,
    keyword_idents,
    trivial_casts
)]
#![warn(
    clippy::all,
    clippy::correctness,
    clippy::style,
    clippy::complexity,
    clippy::perf,
    clippy::pedantic,
    clippy::nursery,
    clippy::cargo
)]
#![allow(clippy::missing_errors_doc)]
#![feature(drain_filter)]

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use rand::prelude::*;

use futures::{prelude::*, stream::FuturesUnordered};

use tokio::{
    fs::File,
    prelude::*,
    task::{self, JoinHandle},
};

use reqwest::Client;
use url::{Host, ParseError, Url};

pub mod error;
pub mod logging;

#[allow(unused)]
use color_eyre::{Help, Report, Result};
#[allow(unused)]
use eyre::{eyre, WrapErr};

#[allow(unused)]
use tracing::{
    debug, debug_span, error, error_span, info, info_span, instrument, trace, trace_span, warn,
    warn_span,
};

// constants
const GET_REQUEST_TIMEOUT: Duration = Duration::from_millis(20000);
const MAX_RECURSION_DEPTH: u8 = 4;
const MAX_HOST_VISITORS: u32 = 512;

static USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (X11; Linux x86_64; rv:76.0) Gecko/20100101 Firefox/76.0",
    "Mozilla/5.0 CK={} (Windows NT 6.1; WOW64; Trident/7.0; rv:11.0) like Gecko",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 12_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/74.0.3729.169 Safari/537.36",
    "Mozilla/5.0 (Windows NT 5.1; rv:11.0) Gecko Firefox/11.0 (via ggpht.com GoogleImageProxy)",
    "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
    "Mozilla/5.0 (Linux; Android 6.0.1; Nexus 5X Build/MMB29P) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/41.0.2272.96 Mobile Safari/537.36 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
];

#[tokio::main]
async fn main() -> Result<()> {
    logging::setup();
    let AppInput {
        inital_urls,
        max_recursion_depth,
    } = setup_app();
    run_crawler(inital_urls, max_recursion_depth).await?;

    Ok(())
}

#[instrument]
async fn run_crawler(inital_urls: HashSet<Url>, max_recursion_depth: u8) -> Result<()> {
    info!("crawling these urls:\n{:?}", &inital_urls);
    let mut dispatcher = Dispatcher::new(inital_urls, max_recursion_depth)?;
    dispatcher.run().await;
    Ok(())
}

type SpiderHandle = JoinHandle<Result<SpiderResponse>>;
type FetchHandle = JoinHandle<Result<()>>;

#[derive(Debug)]
struct Dispatcher {
    client: Client,
    inital_urls: HashSet<Url>,
    max_recursion_depth: u8,
    archive: HashSet<Finding>,
    host_visits: HashMap<Host, u32>,

    spiders: FuturesUnordered<SpiderHandle>,
    fetchers: FuturesUnordered<FetchHandle>,
}

// page can be crawled
// resource can be fetched
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Finding {
    Page(Url, u8),
    Image(Url),
}

struct SpiderResponse {
    new_findings: HashSet<Finding>,
    crawl_link: Url,
    depth: u8,
}

impl Dispatcher {
    fn new(inital_urls: HashSet<Url>, max_recursion_depth: u8) -> Result<Self> {
        let mut rng = rand::thread_rng();

        let client = Client::builder()
            .connect_timeout(GET_REQUEST_TIMEOUT)
            .user_agent(*USER_AGENTS.choose(&mut rng).unwrap())
            .build()?;

        Ok(Self {
            client,
            inital_urls,
            max_recursion_depth,
            archive: HashSet::new(),
            host_visits: HashMap::new(),
            spiders: FuturesUnordered::new(),
            fetchers: FuturesUnordered::new(),
        })
    }

    #[instrument]
    async fn run(&mut self) {
        let mut queue: Vec<Finding> = self
            .inital_urls
            .iter()
            .cloned()
            .map(|u| Finding::Page(u, 0))
            .collect();
        let (mut spiders_finished, mut fetchers_finished) = (false, false);

        // main loop
        while !(spiders_finished && fetchers_finished) {
            // dispatching loop
            queue.drain_filter(|f| {
                let url = match &f {
                    Finding::Page(url, _) | Finding::Image(url) => url,
                };
                let host = url.host().expect("url must have host").to_owned();
                let visits = self.host_visits.entry(host.clone()).or_insert(0);

                if *visits < MAX_HOST_VISITORS {
                    *visits += 1;
                    match f {
                        Finding::Page(_, ref depth) => self.spiders.push(task::spawn(spider_page(
                            url.clone(),
                            self.client.clone(),
                            *depth,
                        ))),
                        Finding::Image(..) => self
                            .fetchers
                            .push(task::spawn(fetch(url.clone(), self.client.clone()))),
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
            spiders_finished = true;
            fetchers_finished = true;

            // crawl loop
            loop {
                match self.spiders.next().await {
                    None => break,
                    Some(Err(e)) => warn!("{}", e),
                    Some(Ok(Err(e))) => warn!("{}", e),
                    Some(Ok(Ok(crawl_response))) => {
                        spiders_finished = false;
                        let SpiderResponse {
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
            }

            // fetch loop
            loop {
                match self.fetchers.next().await {
                    None => break,
                    Some(Err(e)) => warn!("{}", e),
                    Some(Ok(Err(e))) => warn!("{}", e),
                    Some(Ok(Ok(_))) => {
                        fetchers_finished = false;
                    }
                }
            }
        }
    }
}

#[instrument]
async fn spider_page(crawl_link: Url, client: Client, depth: u8) -> Result<SpiderResponse> {
    info!("crawling url `{}`", &crawl_link);

    let request = client.get(crawl_link.clone());
    let response = request.send().await?;
    let body = response.text().await?;

    let new_findings = process_page(&crawl_link, body, depth);
    Ok(SpiderResponse {
        new_findings,
        crawl_link,
        depth,
    })
}

#[instrument]
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
    const fn new(depth: u8) -> Self {
        Self {
            depth,
            page_links: Vec::new(),
            image_links: Vec::new(),
        }
    }
}

impl Aggregate {
    #[instrument]
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

#[instrument]
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

use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};

impl TokenSink for &mut Aggregate {
    type Handle = ();

    #[instrument]
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
                    for attribute in &tag.attrs {
                        if attribute.name.local.as_ref() == "href" {
                            let url_str: &[u8] = attribute.value.borrow();
                            let url_string = String::from_utf8_lossy(url_str).into_owned();
                            self.page_links.push(url_string);
                        }
                    }
                }
                "img" => {
                    for attribute in &tag.attrs {
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

#[instrument]
async fn fetch(resource_url: Url, client: Client) -> Result<()> {
    info!("fetching `{}`", resource_url);
    let path_segments = if let Some(segs) = resource_url.path_segments() {
        segs
    } else {
        return Ok(());
    };
    let file_path = path_segments.last().unwrap();
    let file_path = format!("archive/res/{}", file_path);
    let mut file = File::create(&file_path).await?;

    let request = client.get(resource_url.clone());
    let response = request.send().await?;
    let bytes = response.bytes().await?;

    file.write_all(&bytes).await?;
    Ok(())
}

struct AppInput {
    inital_urls: HashSet<Url>,
    max_recursion_depth: u8,
}

#[instrument]
fn setup_app() -> AppInput {
    use clap::Arg;

    if atty::is(atty::Stream::Stdout) {
        info!("connected to terminal");
    } else {
        info!("not connected to terminal");
    }

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
                .takes_value(true),
        )
        .arg(Arg::with_name("verbose").short("v").long("verbose"));
    // TODO: use verbose

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
