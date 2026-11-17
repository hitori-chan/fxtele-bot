# Dashboard

## Sign-in

One of:

- **The password.** It has 12 to 128 characters, and [zxcvbn](https://github.com/dropbox/zxcvbn)
  mustn't find it easy to guess (no rules about symbols or digits; three or four unrelated words
  pass). It's stored as an argon2id hash. An authenticator app is optional: set up from a QR code
  on the Security page, it then asks for a code on a second page once the password is right
  (a ticket for that browser, good for 5 minutes and 5 tries), and each code works once.
  Changing or removing the password takes the current one; turning the app off takes a code
  from it (a setup link's session skips both, for a forgotten password or lost phone).
  Removing the password removes the app too.
- **A passkey**, tied to `DASHBOARD_URL`'s hostname. The device always checks a PIN or biometric
  (user verification is required).
- **Cloudflare Access**, with `ACCESS_TEAM` and `ACCESS_AUD` set: a valid Access token signs you
  in, so its policy decides what it takes.

Limits:

- Sessions end after an hour unused (a page refreshing itself isn't use), or a day after signing in. A new or removed password, or a
  removed passkey, signs every other browser out and ends sign-ins waiting for a code.
- Adding or removing a passkey, turning the authenticator app on or off, or setting a first
  password needs a sign-in from the last 15 minutes, so a session cookie someone took can't add its own way
  in. The password can't be removed while the app is on: turning the app off takes a code.
- A passkey sign-in works only while that passkey is still saved.
- Setup links, printed by the server (at start while no way to sign in is set up and Cloudflare
  Access is off, or by `server setup-link`), work once within 30 minutes. Their session reaches only the
  Security page, for an hour: enough to set up one of the ways above.
- Five failures within a minute lock the address out (for IPv6, its /64): for 1 minute, then 2,
  4... up to an hour. Wrong setup links don't count, as any web page could make a browser open
  one. Ten wrong codes after the right password, from anywhere, pause password sign-in for an hour
  from the tenth. Codes are checked one at a time, so guesses sent at once can't outrun the limits.
- Five passkey sign-ins at most can be under way from one address.
- Passwords are checked two at a time, so a flood of sign-ins can't exhaust the server's memory.
- Changes are accepted only from the dashboard's own pages, and the last way to sign in can't be
  removed.

## Pages

| Page | |
|---|---|
| Overview | For every site or one, over 1 hour to 90 days: what needs a look (only when something does); success rate, Telegram's requests, answer time (p95, cache hits included) and fetches to the sites, each against the period before; requests by outcome and fetch times (median, p95) over time; top issues; each site, with the busiest minute against its budget, opening on a click to its kinds of post; the photos, videos (1080p and fallbacks) and mosaics Telegram fetched after the previews. CSV export |
| Requests | The latest 500 requests, live, by site and kind (Telegram, failed, people), searchable; each opens in the playground. In memory only |
| Issues | Failed extractions grouped by site, step and message (numbers aside): how many, first and last seen, a trend, what usually causes it and what to do, the latest links; marking one fixed clears it until it fails again |
| Playground | Try a link: Telegram's chat with the bot's message showing the link's preview as Telegram draws it, and Instant View opening when its button is tapped, laid out as Telegram Desktop shows it (its window, sizes and date line), in Telegram's day or night colours; each step's time; the preview tags; the post as JSON; recent links |
| Facebook, Instagram | Per site: status, its last day in numbers, proxy, cookie policy, budget, site name, about the author; cookies from a file (chosen or dropped) or pasted, listed by name and expiry, never by value |
| Test links | One list, failing first: add links (one or several pasted at once; each is checked as it's added), and per link its latest check or why it failed, a day of history, check now, open in the playground, remove. Passing count, uptime, the next check, and which kinds of post each site has covered |
| Alerts | The chat alerts go to, a test alert, what's watched and how each is doing, the latest sent |
| Cache | Every cached post and failure, by site, searchable; memory in use; forget one, or clear everything; how long and how much |
| Previews | Description length, photo mosaics, failure answer, media links, Instant View, counts |
| Videos | 720p or 1080p, size limit, space and days in the bucket, how the bucket is doing, failures |
| Rate limits | Per-IP limit, extraction timeout, each site's budget and how much is in use |
| Telegram IPs | Ranges, the client IP source, requests that claimed to be Telegram but weren't |
| Security | Password, authenticator app, passkeys, Cloudflare Access; sessions, each with a sign-out |
| Activity | Who changed what (how they signed in, or Access's email), every sign-in and refused one with browser and address, and every alert; the latest 1,000 |
| Backup | The settings as JSON, to download or import |

The top bar shows what needs attention (each site's previews and cookies, test links, videos,
Telegram's addresses, alerts), and the menu a dot per site. A line on the overview lists what's
left to set up (test links, alerts, Instagram's host) until it's done.

An import is checked like the settings pages check them, settings the file lacks get their
defaults, and exports from before Instagram load too. The export holds the proxy address (and any
password in it), never cookies or logins.

## How it's built

Pages are rendered by the server (Askama) and work without JavaScript. htmx, vendored in
`server/static/` with no build step, changes pages and sends forms without reloading, refreshes
the overview's numbers every minute and the status every minute, and searches as you type.
Charts are SVG drawn by the server; scripts add tooltips. New requests arrive as server-sent
events. Light and dark follow the system, or the Theme switch under the menu, kept in the
browser; the playground's Telegram follows it too. Times are in the browser's time zone (UTC in CSV exports). Colours meet WCAG 2.2 AA in both. A state
always looks the same: a pill right after the name it's about, marked with a tick (on, working),
a cross (failing), "!" (needs a look) or an empty ring (off), so the shape says it as well as the
colour. Facebook's and Instagram's logos are Simple Icons' (CC0). On
phones the menu slides in from a button and tables become stacked rows. The Content Security
Policy allows no inline scripts or styles.
