# Crawler

## TODO

- [x] try crawling for image_urls
- [x] collect all findings from all tasks using _shared-state concurrency_
- [x] download found images
- [x] transfer to _tokio_ from _async-std_
- [ ] avoid being blocked by websites
- [ ] refactor into _message-passing concurrency_

### Github Code Crawler

- [ ] crawl github for source code
- [ ] "parse" some code (e.g. count pattern occurences)
- [ ] create statistics

## Avoid being blacklisted

- [ ] specify `User-Agent` (Googlebot/2.1 Firefox...)
- [ ] respect `robots.txt`
- [ ] slow down request frequency
- [ ] mimic human behaviour -> randomness
- [ ] random sleep delays
- [ ] Disguise your requests by rotating IPs or Proxy Services
Use a headless browser
Beware of Honey Pot Traps (nofollow tag, display:none)

## Signs of being blacklisted

- CAPTCHA pages
- Unusual content delivery delays
- Frequent response with HTTP 404, 301 or 50x errors
- 301 Moved Temporarily
- 401 Unauthorized
- 403 Forbidden
- 404 Not Found
- 408 Request Timeout
- 429 Too Many Requests
- 503 Service Unavailable
