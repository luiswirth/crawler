use std::{
  borrow::Borrow,
  collections::{HashMap, HashSet},
  time::Duration,
};

use clap::Parser;

use futures::{prelude::*, stream::FuturesUnordered};
use reqwest::Client;
use tokio::{
  fs::File,
  io::AsyncWriteExt,
  task::{self, JoinHandle},
};

use url::{Host, ParseError, Url};

use color_eyre::Result;
use tracing::{info, warn};

const TIMEOUT_DURATION: Duration = Duration::from_millis(5000);
const DEFAULT_RECURSION_DEPTH_LIMIT: u8 = 4;
const HOST_VISIT_LIMIT: u32 = 256;

#[tokio::main]
async fn main() -> Result<()> {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::INFO)
    .init();

  let AppInput {
    inital_urls,
    recursion_depth_limit,
  } = parse_cli_args();

  let mut dispatcher = Dispatcher::new(inital_urls, recursion_depth_limit)?;
  dispatcher.run().await;

  Ok(())
}

type SpiderHandle = JoinHandle<Result<SpiderResponse>>;
type FetchHandle = JoinHandle<Result<()>>;

#[derive(Debug)]
struct Dispatcher {
  client: Client,
  inital_urls: HashSet<Url>,
  recursion_depth_limit: u8,
  archive: HashSet<Finding>,
  host_visits: HashMap<Host, u32>,

  spiders: FuturesUnordered<SpiderHandle>,
  fetchers: FuturesUnordered<FetchHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Finding {
  Page(Url, u8),
  Image(Url),
}

struct SpiderResponse {
  findings: HashSet<Finding>,
  depth: u8,
}

impl Dispatcher {
  fn new(inital_urls: HashSet<Url>, recursion_depth_limit: u8) -> Result<Self> {
    let client = Client::builder()
      .connect_timeout(TIMEOUT_DURATION)
      .build()?;

    Ok(Self {
      client,
      inital_urls,
      recursion_depth_limit,
      archive: Default::default(),
      host_visits: Default::default(),
      spiders: Default::default(),
      fetchers: Default::default(),
    })
  }

  async fn run(&mut self) {
    let mut queue: Vec<Finding> = self
      .inital_urls
      .iter()
      .cloned()
      .map(|u| Finding::Page(u, 0))
      .collect();

    while !queue.is_empty() || !self.spiders.is_empty() || !self.fetchers.is_empty() {
      for finding in queue.drain(..) {
        let url = match &finding {
          Finding::Page(url, _) | Finding::Image(url) => url,
        };

        let Some(host) = url.host().map(|h| h.to_owned()) else {
          continue
        };
        let visits = self.host_visits.entry(host.clone()).or_insert(0);
        if *visits > HOST_VISIT_LIMIT {
          continue;
        }
        *visits += 1;

        match finding {
          Finding::Page(_, ref depth) => self.spiders.push(task::spawn(spider_page(
            url.clone(),
            self.client.clone(),
            *depth,
          ))),
          Finding::Image(..) => self
            .fetchers
            .push(task::spawn(fetch(url.clone(), self.client.clone()))),
        };
      }

      while let Some(spider) = self.spiders.next().await {
        let spider = spider.unwrap();

        match spider {
          Ok(SpiderResponse {
            mut findings,
            depth,
          }) => {
            findings = findings.difference(&self.archive).cloned().collect();
            self.archive.extend(findings.clone());

            if depth < self.recursion_depth_limit {
              queue.extend(findings);
            }
          }
          Err(e) => warn!("Spider failed with error: {}", e),
        }
      }

      while let Some(fetcher) = self.fetchers.next().await {
        let fetcher = fetcher.unwrap();
        if let Err(e) = fetcher {
          warn!("Fetcher failed with error: {}", e);
        }
      }
    }
  }
}

async fn spider_page(url: Url, client: Client, depth: u8) -> Result<SpiderResponse> {
  info!("crawling url `{}`", &url);

  let request = client.get(url.clone());
  let response = request.send().await?;
  let body = response.text().await?;

  let findings = process_page(&url, body, depth);
  Ok(SpiderResponse { findings, depth })
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
  const fn new(depth: u8) -> Self {
    Self {
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

use html5ever::tokenizer::{
  BufferQueue, Tag, TagKind, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};

impl TokenSink for &mut Aggregate {
  type Handle = ();

  fn process_token(&mut self, token: Token, _line_number: u64) -> TokenSinkResult<Self::Handle> {
    if let TagToken(
      ref tag @ Tag {
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

async fn fetch(resource_url: Url, client: Client) -> Result<()> {
  info!("fetching `{}`", resource_url);

  let request = client.get(resource_url.clone());
  let response = request.send().await?;
  let bytes = response.bytes().await?;

  let Some(url_segments) = resource_url.path_segments() else {
    return Ok(());
  };
  let file_path = url_segments.last().unwrap();
  let file_path = format!("prey/res/{}", file_path);
  let mut file = File::create(&file_path).await?;

  file.write_all(&bytes).await?;

  Ok(())
}

struct AppInput {
  inital_urls: HashSet<Url>,
  recursion_depth_limit: u8,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
  urls: Vec<String>,

  #[arg(short, long, default_value_t = DEFAULT_RECURSION_DEPTH_LIMIT)]
  recursion_depth_limit: u8,
}

fn parse_cli_args() -> AppInput {
  let args = Args::parse();

  let recursion_depth_limit = args.recursion_depth_limit;
  let inital_urls: HashSet<Url> = args
    .urls
    .iter()
    .map(AsRef::as_ref)
    .map(Url::parse)
    .map(Result::unwrap)
    .collect();

  AppInput {
    inital_urls,
    recursion_depth_limit,
  }
}
