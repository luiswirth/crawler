use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use std::borrow::Borrow;
use url::{ParseError, Url};

use async_std::task;
use surf;

type Result = std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>>;
type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result> + Send>>;

#[derive(Debug)]
enum Link {
    Unparsed(String),
    Parsed(Url),
}

#[derive(Default, Debug)]
struct Findings {
    page_links: Vec<Link>,
    image_links: Vec<Link>,
}

impl TokenSink for &mut Findings {
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
                            self.page_links.push(Link::Unparsed(
                                String::from_utf8_lossy(url_str).into_owned(),
                            ));
                        }
                    }
                }
                "img" => {
                    for attribute in tag.attrs.iter() {
                        if attribute.name.local.as_ref() == "src" {
                            let url_str: &[u8] = attribute.value.borrow();
                            self.image_links.push(Link::Unparsed(
                                String::from_utf8_lossy(url_str).into_owned(),
                            ));
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

impl Link {
    fn parse(&mut self, page_url: &Url) {
        if let Link::Unparsed(link) = self {
            *self = Link::Parsed(match Url::parse(link) {
                Err(ParseError::RelativeUrlWithoutBase) => page_url.join(link).unwrap(),
                Err(_) => panic!("Malformed link found: {}", link),
                Ok(url) => url,
            });
        } else {
            panic!("link already parsed");
        }
    }
}

impl Findings {
    fn parse(&mut self, page_url: &Url) {
        self.page_links.iter_mut().for_each(|l| l.parse(&page_url));
        self.image_links.iter_mut().for_each(|l| l.parse(&page_url));
    }
}

fn process_page(page_url: &Url, page_body: String) -> Findings {
    let mut page_url = page_url.clone();
    page_url.set_path("");
    page_url.set_query(None);

    let mut findings = Findings::default();
    let mut tokenizer = Tokenizer::new(&mut findings, TokenizerOpts::default());
    let mut buffer = BufferQueue::new();
    buffer.push_back(page_body.into());
    let _ = tokenizer.feed(&mut buffer);

    findings.parse(&page_url);
    findings
}

async fn crawl_loop(pages: Vec<Url>, current: u8, max: u8) -> Result {
    println!("Current recursion depth: {}, max depth: {}", current, max);

    if current >= max {
        println!("Reached max depth");
        return Ok(());
    }

    let mut tasks = Vec::new();
    for url in pages {
        let task = task::spawn(async move {
            let mut page = surf::get(&url).await?;
            let body = page.body_string().await?;
            let new_findings = process_page(&url, body);

            // add new_findings to findings!

            let new_page_links = new_findings
                .page_links
                .iter()
                .map(|l| match l {
                    Link::Parsed(link) => link.clone(),
                    Link::Unparsed(_) => unreachable!(),
                })
                .collect::<Vec<Url>>();

            box_crawl_loop(new_page_links, current + 1, max).await
        });
        println!("new task");
        tasks.push(task);
    }

    for task in tasks.into_iter() {
        task.await?;
        println!("task done");
    }

    Ok(())
}

// wraper function for pinning
fn box_crawl_loop(pages: Vec<Url>, current: u8, max: u8) -> BoxFuture {
    Box::pin(crawl_loop(pages, current, max))
}

async fn fetch_images() -> Result {
    let url = Url::parse("").unwrap();
    let mut res = surf::get(&url).await?;
    let img_bytes = res.body_bytes().await?;
    dbg!(img_bytes);
    Ok(())
}


// create findings here and pass by mutable reference to crawl_loop
// in there aggregate all findings
// have seperate array with all new found page_link to continue crawling

//pub async fn crawl(pages: Vec<Url>, max_recursion: u8) -> BoxFuture {
//box_crawl_loop(pages, max_recursion, 0)
//}

fn main() -> Result {
    task::block_on(async {
        let urls = vec![Url::parse("https://www.rust-lang.org").unwrap()];
        box_crawl_loop(urls, 0, 2).await.unwrap();
        println!("done");
    });
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
