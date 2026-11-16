# Dashboard

## Sign-in

Always with two factors, one of:

- **A passkey**, tied to `DASHBOARD_URL`'s hostname. The device always checks a PIN or biometric
  (user verification is required), so a passkey is two factors by itself.
- **The password and an authenticator app.** The password has 12 to 128 characters, and
  [zxcvbn](https://github.com/dropbox/zxcvbn) mustn't find it easy to guess (no rules about
  symbols or digits; three or four unrelated words pass). It's stored as an argon2id hash. The
  app is set up from a QR code on the Login page, and each code works once. A password without
  the app can't sign in, and removing the password removes the app too.
- **Cloudflare Access**, with `ACCESS_TEAM` and `ACCESS_AUD` set: a valid Access token signs you
  in. The server can't see how you signed in to Access, so require MFA in its policy (a rule on
  the authentication method, or an identity provider that enforces it).

Limits:

- Sessions end after an hour unused, or a day after signing in (NIST SP 800-63B's guidance for
  two factors). A new password signs every other browser out.
- Setup links, printed by the server, work once within 30 minutes. Their session reaches only the
  Login page, for an hour: enough to set up one of the ways above.
- Five failures within a minute lock the address out (for IPv6, its /64): for 1 minute, then 2,
  4... up to an hour. Ten wrong codes after the right password, from anywhere, pause password
  sign-in for an hour.
- Passwords are checked two at a time, so a flood of sign-ins can't exhaust the server's memory.
- Changes are accepted only from the dashboard's own pages, and the last way to sign in can't be
  removed.

## Pages

| Page | |
|---|---|
| Overview | Health at a glance; requests over 1 hour to 90 days by outcome, and by page type with trends; a live feed you can filter and pause; a box to try a link |
| Playground | Try a link: timings, the preview Telegram would show, its tags, the Instant View page, the post as JSON |
| Failures | Recent failed extractions, filtered by page type and step |
| Test links and alerts | Links, alert chat, run now, send a test alert |
| Cache | Every cached post and failure, searchable; forget one, or clear everything; how long and how much |
| Facebook | Proxy, cookie policy, extraction timeout; cookies from a file (chosen or dropped) or pasted, listed by name and expiry, never by value |
| Previews | Site name, description length, failure answer, media links, Instant View, counts, author pictures |
| Videos | 720p or 1080p, size limit, space and days in the bucket, how the bucket is doing, failures |
| Rate limits | Per-IP and Facebook limits, budget in use |
| Telegram IPs | Ranges, the client IP source, requests that claimed to be Telegram but weren't |
| Login | Password, authenticator app, passkeys, Cloudflare Access; sessions, each with a sign-out |
| Backup | The settings as JSON, to download or import |

An import is checked like the settings pages check them, and settings the file lacks get their
defaults. The export holds the proxy address (and any password in it), never cookies or logins.

## How it's built

Pages are rendered by the server (Askama) and work without JavaScript. htmx, vendored in
`server/static/` with no build step, changes pages and sends forms without reloading, refreshes
the chart every minute and searches the cache as you type. Charts are SVG drawn by the server.
Light and dark follow the system, or the choice under the menu, kept in the browser. Request
statistics download as CSV from the Overview's chart.
