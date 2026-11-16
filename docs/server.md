# Server

## A Telegram request

1. Telegram's fetcher asks for `https://fb.example.com/reel/123`.
2. The server checks it's Telegram: the user agent contains `TelegramBot` **and** the IP is in
   Telegram's ranges (`core.telegram.org/resources/cidr.txt`, refreshed daily, with a bundled
   copy as fallback). Anything else, a spoofed user agent included, is redirected to facebook.com
   and never causes a fetch.
3. The path becomes a page type and a cache key ([Facebook extraction](facebook.md)).
4. On a cache miss, extraction takes a slot from the Facebook budget (waiting up to 2 s) and must
   finish within the extraction timeout. Concurrent requests for one post share one fetch.
5. The answer is one page: preview tags in `<head>`, the Instant View article in `<body>`,
   compressed and marked `Cache-Control: no-store, no-transform`.

## Preview tags

- `og:site_name`, `og:title`, `og:description` (cut to the description length).
- Video: `og:type=video.other`, `og:video` (`video/mp4`) with its size; the poster in `og:image`.
- Photos: `og:image` is the first. With several, the description starts with `1/N`.
- After a failure, Telegram gets what "When extraction fails" says. Telegram keeps the first
  answer for a URL, so failures are cached for only 5 minutes.

## Instant View

Telegram applies its Medium template to pages that present themselves as Medium articles
(`al:android:app_name=Medium` plus `article:published_time`), as FxEmbed does. The article has
the title, author and date, the full text with links, every photo or the video, and a "View on
Facebook" link. It can be turned off per post type. Telegram doesn't document this, so it may
stop working.

## Media links

With the `/_/m/` setting, media URLs point at the server, which redirects to a fresh CDN URL,
refetching the post if the old one expired (for Telegram only). Only media of posts the server
extracted itself are served.

## Facebook requests

- Only to `facebook.com` hosts; redirects are followed by hand, at most 10, and must stay on
  Facebook.
- One warm HTTP/2 connection, direct or through the proxy setting.
- Cookie policy `fallback` (the default) sends cookies only after a login wall, keeping your
  account out of most fetches. Story cards always hit the wall, so they go logged in right away.
- Cookies live in `server.db` and are never logged. A paste without `c_user` and `xs` is
  refused.

## Limits and cache

| | Default | |
|---|---|---|
| Requests per IP | 120/min | Public listener; Telegram exempt; over it: `429` |
| Facebook fetches | 60/min, 4 at once | Cache misses from Telegram, the playground and test links |
| Cache | 64 MB | Posts 60 min (less if a media URL expires sooner), failures 5 min |

## Statistics

Every request is counted by outcome (redirect, hit, extracted, failed at a stage, rate limited)
and page type, with timings, and saved each minute: per-minute for 7 days, hourly for 90 days.
The last 500 failures are kept with their URL and error.

## Test links and alerts

Test links are checked 2 minutes after start, then every 30 minutes, within the Facebook budget.
Alerts go to the alert chat, sent with `TELEGRAM_BOT_TOKEN`, when:

- a test link starts failing, or works again
- Facebook rejects the cookies, or they expire within 7 days
- the Facebook budget ran out more than 10 times in half an hour
