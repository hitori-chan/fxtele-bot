# Dashboard

## Sign-in

Any of these, alone or together:

- **Password**, at least 10 characters (stored as an argon2id hash). Optionally with an
  **authenticator app**: scan the QR code on the Login page; sign-in then also asks for the
  6-digit code, and each code works once.
- **Passkeys**, tied to `DASHBOARD_URL`'s hostname.
- **Cloudflare Access**: with `ACCESS_TEAM` and `ACCESS_AUD` set, a valid Access token signs you
  in.

Sessions last 30 days. Setup links work once, within 30 minutes. Five failures within a minute
lock the IP out for 1 minute, then 2, 4... up to an hour. Changes are accepted only from the
dashboard's own pages, and the last way to sign in can't be removed.

## Pages

| Page | |
|---|---|
| Overview | Health at a glance; requests over 1 hour to 90 days by outcome, and by page type with trends; a live feed you can filter and pause; a box to try a link |
| Playground | Try a link: timings, the preview Telegram would show, its tags, the Instant View page, the post as JSON |
| Failures | Recent failed extractions, filtered by page type and step |
| Test links | Links, alert chat, run now |
| Cache | Every cached post and failure, searchable; forget one, or clear everything; how long and how much |
| Facebook | Proxy, cookie policy, extraction timeout, cookies |
| Previews | Site name, description length, failure answer, media links, Instant View |
| Videos | 720p or 1080p, how the bucket is doing, its files, failures |
| Rate limits | Per-IP and Facebook limits, budget in use |
| Telegram IPs | Ranges, the client IP source, requests that claimed to be Telegram but weren't |
| Login | Password, authenticator app, passkeys, Cloudflare Access, sessions |
| Import and export | The settings as JSON, to download or import; request statistics as CSV |

An import is checked like the settings pages check them, and settings the file lacks get their
defaults. The export holds the proxy address (and any password in it), never cookies or logins.

## How it's built

Pages are rendered by the server (Askama) and work without JavaScript. htmx, vendored in
`server/static/` with no build step, changes pages and sends forms without reloading, refreshes
the chart every minute and searches the cache as you type. Charts are SVG drawn by the server.
Light and dark follow the system, or the choice under the menu, kept in the browser.

Keys: `/` jumps to the page's search or link box, `g` then the letter beside a menu entry goes
there, `t` switches the theme, `?` lists them.
