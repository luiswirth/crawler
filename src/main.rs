use log::{debug, error, info, trace, warn};
use chrono;

use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use url::{ParseError, Url};

use surf;

use async_std::{
    future,
    task, sync::{Arc, Mutex},
    fs::File,
    io::prelude::*,
};

use std::borrow::Borrow;
use std::time::Duration;

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
type Result<T> = std::result::Result<T, Error>;
type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>>;

// goal: use _shared-state concurrency_ (multiple ownership) to accumulate the findings
// from all tasks into one collection.

const GET_REQUEST_TIMEOUT: Duration = Duration::from_micros(60000);

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
        .map(|l| match Url::parse(&l) {
            Err(ParseError::RelativeUrlWithoutBase) => page_url.join(&l).unwrap(),
            Err(_) => panic!("Malformed link found: {}", l),
            Ok(url) => url,
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

    let findings = findings.parse(&page_url);
    findings
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
            let timeout_future = future::timeout(GET_REQUEST_TIMEOUT, surf::get(&url));
            let request = match timeout_future.await {
                Ok(request) => request,
                Err(_) => {
                    error!("Error: GET-request timeout with url `{}`", &url);
                    return Ok(());
                }
            };
            let mut page = request?;
            let body = page.body_string().await?;
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
            box_crawl_loop(new_page_links, current + 1, max, findings).await
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
	    let path_segments = url.path_segments().ok_or_else(|| "cannot be base")?;
	    let file_path = path_segments.last().unwrap();
	    let file_path = format!("imgs/{}", file_path);
	    let mut file = File::create(file_path).await?;


	    let timeout_future = future::timeout(GET_REQUEST_TIMEOUT, surf::get(&url));
	    let request = match timeout_future.await {
		Ok(request) => request,
		Err(_) => {
		    error!("Error: GET-request timeout with url `{}`", &url);
		    return Ok::<(), Error>(());
		}
	    };

	    let mut res = request?;
	    let img_bytes: Vec<u8> = res.body_bytes().await?;
	    let _ = file.write(&img_bytes).await?;
	    Ok(())
	});
	tasks.push(task);
    }
    for task in tasks.into_iter() {
	task.await?;
    }
    Ok(())
}

pub fn crawl(pages: Vec<Url>, max_recursion: u8) {
    let findings = Arc::new(Mutex::new(Findings::default()));
    task::block_on(async {
        box_crawl_loop(pages, 0, max_recursion, Arc::clone(&findings))
            .await
            .unwrap();
	let findings = Arc::try_unwrap(findings).unwrap(); // remove Arc hull
	let findings_inner = findings.into_inner(); // remove Mutex hull
	fetch_images(findings_inner.image_links).await.unwrap();
    });
}


fn setup_logger() -> std::result::Result<(), fern::InitError> {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S]"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(log::LevelFilter::Trace)
        .chain(std::io::stdout())
        .chain(fern::log_file("log/crawler.log")?)
        .apply()?;
    Ok(())
}

fn main() -> Result<()> {
    setup_logger()?;
    let urls = vec![Url::parse("https://bing.com").unwrap()];
    crawl(urls, 2);
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

    #[test]
    fn crawl_rust_website() {
        task::block_on(async {
            let urls = vec![Url::parse("https://www.rust-lang.org").unwrap()];
            box_crawl(urls, 1, 2).await.unwrap();
        });
    }
}
