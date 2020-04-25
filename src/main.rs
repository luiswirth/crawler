//!
#![warn(missing_debug_implementations, rust_2018_idioms)]

use chrono;
use std::{borrow::Borrow, collections::HashSet, time::Duration};

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

#[derive(Default, Debug)]
struct Root {
    inital_urls: Vec<Url>,
    max_recursion_depth: u8,
    archive: Findings,
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
        .collect::<Vec<Url>>();

    root
}

// schedules and dispatches crawlers on pages
// keep track of urls already crawled to avoid recrawling
// keep track of amount of crawlers on _domain_ to avoid overloading page
struct Dispatcher {
    root: Root,
    crawlers: Vec<JoinHandle<Result<Findings, CrawlError>>>, // or Vec<Task>
                                                             //downloaders: Vec<JoinHandle<()>>, // unit to fetch images
}

impl Dispatcher {
    fn new(root: Root) -> Dispatcher {
        Dispatcher {
            root,
            crawlers: Vec::new(),
        }
    }

    async fn run(&mut self) {
        for url in &self.root.inital_urls {
            self.root.archive.domains.insert(url_to_domain(url.clone()));
            self.root.archive.page_links.insert(url.clone());
        }

        // await crawlers to gather findings
        // later: use Channel for _message-passing_ concurrency

        // generalize to findings
        let mut next_links: HashSet<Url> = self.root.inital_urls.clone().into_iter().collect();
        let mut link_archive: HashSet<Url> = HashSet::new();

        for _depth in 0..self.root.max_recursion_depth {
            for url in &next_links {
                self.crawlers.push(task::spawn(crawl_page(url.clone())));
            }

            for crawler in self.crawlers.drain(..) {
                let crawl_result = crawler.await;
                match crawl_result {
                    Err(e) => warn!("{}", &e),
                    Ok(new_findings) => {
                        // TODO: optimize
                        let difference = new_findings
                            .page_links
                            .difference(&self.root.archive.page_links)
                            .cloned()
                            .collect::<HashSet<_>>();
                        link_archive.extend(difference.clone());
                        next_links = difference;
                    }
                };
            }
        }
    }
}

#[derive(Default, Debug)]
struct RawFindings {
    page_links: Vec<String>,
    image_links: Vec<String>,
}

#[derive(Default, Debug)]
struct Findings {
    domains: HashSet<Url>,
    page_links: HashSet<Url>,
    image_links: HashSet<Url>,
}

impl RawFindings {
    fn parse(self, page_url: &Url) -> Findings {
        let page_links = parse_links(self.page_links, page_url);
        let image_links = parse_links(self.image_links, page_url);
        let domains = page_links
            .iter()
            .map(|u| url_to_domain(u.clone()))
            .collect();
        Findings {
            domains,
            page_links,
            image_links,
        }
    }
}

impl Findings {
    #[allow(dead_code)]
    fn extend(&mut self, other: Self) {
        self.domains.extend(other.domains);
        self.page_links.extend(other.page_links);
        self.image_links.extend(other.image_links);
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
        .collect()
}

fn url_to_domain(mut url: Url) -> Url {
    url.set_path("");
    url.set_query(None);
    url
}

async fn crawl_page(url: Url) -> CrawlResult<Findings> {
    info!("crawling url `{}`", &url);

    let future = timeout(GET_REQUEST_TIMEOUT, surf::get(&url).recv_string());
    let body = match future.await {
        Err(_) => {
            return Err(CrawlError::Timeout);
        }
        Ok(request) => match request {
            Ok(page) => page,
            Err(err) => {
                return Err(CrawlError::Get(err));
            }
        },
    };

    let new_findings = process_page(&url, body);
    Ok(new_findings)
}

fn process_page(page_url: &Url, page_body: String) -> Findings {
    let mut page_url = page_url.clone();
    page_url.set_path("");
    page_url.set_query(None);

    let mut findings = RawFindings::default();
    let mut tokenizer = Tokenizer::new(&mut findings, TokenizerOpts::default());
    let mut buffer = BufferQueue::new();
    buffer.push_back(page_body.into());
    let _ = tokenizer.feed(&mut buffer);

    findings.parse(&page_url)
}

impl TokenSink for &mut RawFindings {
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
    Timeout,
    Get(HttpError),
}

type CrawlResult<T> = Result<T, CrawlError>;

impl std::fmt::Display for CrawlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrawlError::Timeout => write!(f, "future timed out"),
            CrawlError::Get(e) =>
            //e.fmt(f)
            {
                write!(f, "GET error: {}", e)
            }
        }
    }
}

impl std::error::Error for CrawlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CrawlError::Timeout => None,
            CrawlError::Get(e) => Some(e.as_ref()),
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
