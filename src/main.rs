//!
#![warn(missing_debug_implementations, rust_2018_idioms)]

use chrono;
use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    time::Duration,
};

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use async_std::{
    fs::File,
    future::timeout,
    prelude::*,
    //sync::{Arc, Mutex},
    task::{self, JoinHandle},
};
//use futures::channel::mpsc;

use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use surf;
use url::{ParseError, Url};

type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;
type DynResult<T> = std::result::Result<T, DynError>;
//type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = DynResult<T>> + Send>>;

//type Sender<T> = mpsc::UnboundedSender<T>;
//type Receiver<T> = mpsc::UnboundedReceiver<T>;

mod util;

// constants
const GET_REQUEST_TIMEOUT: Duration = Duration::from_millis(4000);

fn main() -> DynResult<()> {
    setup_logger()?;
    let root = setup_app();

    info!("crawling these urls:\n{:?}", root.inital_urls);
    let mut dispatcher = Dispatcher::new(root);

    task::block_on(async move {
        dispatcher.run().await;
    });

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

fn setup_app() -> Root {
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

    let mut root = Root::default();

    let matches = app.get_matches();
    root.max_recursion_depth = matches.value_of("depth").unwrap().parse().unwrap();
    root.inital_urls = matches
        .values_of("url")
        .unwrap()
        .map(|url| match Url::parse(url) {
            Ok(url) => url,
            Err(err) => panic!("{}", err),
        })
        .collect::<HashSet<Url>>();

    root
}

#[derive(Default, Debug)]
struct Root {
    inital_urls: HashSet<Url>,
    max_recursion_depth: u8,
    archive: Vec<Finding>,
}

// schedules and dispatches crawlers on pages
// keep track of urls already crawled to avoid recrawling
// keep track of amount of crawlers on _domain_ to avoid overloading page
struct Dispatcher {
    root: Root,
    crawlers: Vec<JoinHandle<Result<Finding, CrawlError>>>,
    domain_visitors: HashMap<Url, u32>,
    //downloaders: Vec<JoinHandle<()>>, unit to fetch images
}

impl Dispatcher {
    fn new(root: Root) -> Dispatcher {
        Dispatcher {
            root,
            crawlers: Vec::new(),
            domain_visitors: HashMap::new(),
        }
    }

    async fn run(&mut self) {
        let inital_finding = Finding {
            page_links: self.root.inital_urls.clone(),
            image_links: HashSet::new(),
            depth: 0,
        };
        let mut uncrawled_findings: Vec<Finding> = vec![inital_finding];

        loop {
            // schedule crawling tasks
            let mut i = 0; // Vec::drain_filter() is unstable
            while i != uncrawled_findings.len() {
                if uncrawled_findings[i].depth < self.root.max_recursion_depth {
                    let uncrawled = uncrawled_findings.remove(i);
                    for link in &uncrawled.page_links {
                        self.crawlers.push(task::spawn(crawl_page(link.clone())));
                    }
                    self.root.archive.push(uncrawled);
                } else {
                    i += 1;
                }
            }

            for crawler in self.crawlers.drain(..) {
                // await crawlers to gather findings
                // later: use Channel for _message-passing_ concurrency
                let crawl_result = crawler.await;
                match crawl_result {
                    Err(e) => warn!("{}", &e),
                    Ok(new_findings) => {
                        // TODO: optimize

                        let mut difference = new_findings;
                        for finding in &self.root.archive {
                            difference = difference.difference(finding);
                        }

                        uncrawled_findings.push(difference);
                    }
                };
            }
        }
    }
}

#[derive(Default, Debug)]
struct RawFinding {
    page_links: Vec<String>,
    image_links: Vec<String>,
    depth: u8,
}

#[derive(Default, Debug)]
struct Finding {
    page_links: HashSet<Url>,
    image_links: HashSet<Url>,
    depth: u8,
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
    fn extend(&mut self, other: Self) {
        self.page_links.extend(other.page_links);
        self.image_links.extend(other.image_links);
    }

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

fn url_to_domain(mut url: Url) -> Url {
    url.set_path("");
    url.set_query(None);
    url
}

async fn crawl_page(url: Url) -> CrawlResult<Finding> {
    info!("crawling url `{}`", &url);

    let future = timeout(GET_REQUEST_TIMEOUT, surf::get(&url).recv_string());
    let body = match future.await {
        Err(_) => {
            return Err(CrawlError::Timeout(url));
        }
        Ok(request) => match request {
            Ok(page) => page,
            Err(err) => {
                return Err(CrawlError::Get(err, url));
            }
        },
    };

    let new_findings = process_page(&url, body);
    Ok(new_findings)
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

#[allow(dead_code)]
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

use http_types::Error as HttpError;

#[derive(Debug)]
enum CrawlError {
    Timeout(Url),
    Get(HttpError, Url),
}

type CrawlResult<T> = Result<T, CrawlError>;

impl std::fmt::Display for CrawlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrawlError::Timeout(url) => write!(f, "future timed out with url `{}`", url),
            CrawlError::Get(e, url) =>
            //e.fmt(f)
            {
                write!(f, "GET error: {} with url `{}`", e, url)
            }
        }
    }
}

impl std::error::Error for CrawlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CrawlError::Timeout(_) => None,
            CrawlError::Get(e, _) => Some(e.as_ref()),
        }
    }
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
