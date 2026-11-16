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
| Overview | Live requests, last hour and 24 hours by page type, cache, Facebook budget, cookies, test links |
| Playground | Try a link: timings, the preview Telegram would show, its tags, the Instant View page, the post as JSON |
| Failures | Recent failed extractions |
| Facebook | Proxy, cookie policy, cache, preview settings, cookies |
| Rate limits | Per-IP and Facebook limits, budget in use |
| Cache | Forget one link, or clear everything |
| Test links | Links, alert chat, run now |
| Telegram IPs | Ranges, the client IP source, requests that claimed to be Telegram but weren't |
| Login | Password, authenticator app, passkeys, Cloudflare Access, sessions |
