#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import sys
from typing import Any

from playwright.sync_api import (
    Page,
    Response,
    TimeoutError as PlaywrightTimeoutError,
    sync_playwright,
)


# anonyig.com has several specialized sub-pages, each with the same Vue
# search-form component but different default targets:
#   /en1/                            -> Story viewer (placeholder "@username or link")
#   /en/iganony/                     -> Reels & posts downloader  ← what we want
#   /en/instagram-profile-viewer/    -> Profile browser
#   /en/instagram-highlights-viewer/ -> Highlights viewer
# Use /en/iganony/ for arbitrary post / reel URLs.
ANONYIG_URL = "https://anonyig.com/en/iganony/"
CONVERT_API_FRAGMENT = "/api/convert"  # api-wh.anonyig.com/api/convert
DEFAULT_TIMEOUT_MS = 30_000

# Realistic UA — the site's TLS/CDN may filter on this. Match a recent Chrome
# on Windows since that's what the original captured curl used.
USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/147.0.0.0 Safari/537.36"
)


class IGMediaError(Exception):
    """Raised when extraction fails."""


def extract_media(
    post_url: str,
    *,
    headed: bool = False,
    timeout_ms: int = DEFAULT_TIMEOUT_MS,
    debug: bool = False,
) -> dict[str, Any]:
    """
    Resolve an Instagram post URL to direct media URLs by driving anonyig.com.

    Args:
        post_url:    Instagram post / reel / IGTV URL.
        headed:      If True, launch a visible browser window (debugging aid).
        timeout_ms:  Hard ceiling for navigation + form interaction + response
                     capture. The default of 30s is generous; bump it on slow
                     networks. Each individual wait inside is shorter.
        debug:       Print step-by-step progress to stderr.

    Returns:
        A normalized dict (see _normalize for the schema).

    Raises:
        IGMediaError: on any extraction failure.
    """
    log = (lambda msg: print(f"[debug] {msg}", file=sys.stderr)) if debug else (lambda _: None)

    with sync_playwright() as p:
        browser = p.chromium.launch(headless=not headed)
        context = browser.new_context(
            user_agent=USER_AGENT,
            viewport={"width": 1280, "height": 800},
            locale="en-US",
        )
        page = context.new_page()

        # Capture state. We use a list rather than a single var so the
        # response handler can append even after multiple matching calls
        # (which can happen if the page retries).
        captured_responses: list[dict[str, Any]] = []

        def on_response(resp: Response) -> None:
            # Match by URL fragment so we don't depend on subdomain stability.
            if CONVERT_API_FRAGMENT in resp.url and resp.request.method == "POST":
                log(f"intercepted {resp.request.method} {resp.url} -> {resp.status}")
                try:
                    body = resp.json()
                except Exception:
                    try:
                        body = {"_raw_text": resp.text()}
                    except Exception as e:
                        body = {"_error": f"unable to read body: {e}"}
                captured_responses.append({
                    "url": resp.url,
                    "status": resp.status,
                    "body": body,
                })

        page.on("response", on_response)

        try:
            log(f"navigating to {ANONYIG_URL}")
            page.goto(ANONYIG_URL, wait_until="domcontentloaded", timeout=timeout_ms)

            log("locating input field")
            _fill_input(page, post_url, debug=debug)

            log("submitting form")
            _click_submit(page, debug=debug)

            log("waiting for /api/convert response")
            convert_response = _wait_for_convert(
                page, captured_responses, timeout_ms=timeout_ms, debug=debug
            )
        except PlaywrightTimeoutError as e:
            raise IGMediaError(f"timed out interacting with anonyig.com: {e}") from e
        except IGMediaError:
            raise
        except Exception as e:
            raise IGMediaError(f"unexpected error: {e}") from e
        finally:
            context.close()
            browser.close()

    if convert_response["status"] >= 400:
        raise IGMediaError(
            f"anonyig.com /api/convert returned HTTP {convert_response['status']}: "
            f"{convert_response['body']!r}"
        )

    return _normalize(convert_response["body"], post_url)


# ─────────────────────────── selector helpers ────────────────────────────────
# These walk a list of progressively broader selectors. The first match wins.
# This makes the script resilient to small site rewrites (renamed classes etc.)
# without sacrificing the ability to be specific when a stable selector exists.

def _fill_input(page: Page, post_url: str, *, debug: bool = False) -> None:
    """
    Locate the URL input box and fill it.

    The page uses a Vue component <input class="search search-form__input"
    v-model="query" placeholder="@username or link">. We target by class
    name first, with progressively broader fallbacks if the site is rebuilt
    with renamed classes.

    Important: because of v-model, we MUST use locator.fill() (which fires
    'input' events) rather than directly setting the DOM value — otherwise
    Vue's reactivity won't pick up the change and the submit button will
    stay disabled.
    """
    candidates = [
        "input.search-form__input",                # exact class as of writing
        "input.search",                            # the secondary class
        "form.search-form input[type='text']",     # scoped to the form
        "input[placeholder*='link' i]",
        "input[placeholder*='username' i]",
        "input[type='text']:visible",              # last resort
    ]
    last_error: Exception | None = None
    for sel in candidates:
        try:
            loc = page.locator(sel).first
            loc.wait_for(state="visible", timeout=5_000)
            if debug:
                print(f"[debug] input matched by {sel!r}", file=sys.stderr)
            loc.fill(post_url)
            return
        except Exception as e:
            last_error = e
            continue
    raise IGMediaError(
        f"could not find URL input field on {ANONYIG_URL}; "
        f"site layout may have changed (last error: {last_error})"
    )


def _click_submit(page: Page, *, debug: bool = False) -> None:
    """
    Click the submit/download button.

    The page uses <button class="search-form__button" :disabled="disabled"
    @click="handleSearchButtonClick"> — note the form has
    `onsubmit="return false"`, so pressing Enter alone will NOT submit;
    we must click the button. The :disabled binding means we must also
    wait until Vue has decided the input contents are valid.
    """
    candidates = [
        "button.search-form__button:not([disabled])",  # wait until Vue enables it
        "button.search-form__button",
        "form.search-form button",
        "button[class*='search'][class*='button']",   # softer fallback
    ]
    for sel in candidates:
        try:
            loc = page.locator(sel).first
            # Wait for visible AND not disabled — Vue toggles `disabled`
            # via the :disabled binding once it deems the input valid.
            loc.wait_for(state="visible", timeout=5_000)
            if debug:
                print(f"[debug] submit matched by {sel!r}", file=sys.stderr)
            loc.click()
            return
        except Exception:
            continue

    # The form has onsubmit="return false", so pressing Enter does NOT
    # trigger submission. If we can't find the button, we have to fail.
    raise IGMediaError(
        "could not find submit button on the page; "
        "site layout may have changed"
    )


def _wait_for_convert(
    page: Page,
    captured: list[dict[str, Any]],
    *,
    timeout_ms: int,
    debug: bool = False,
) -> dict[str, Any]:
    """
    Wait until the /api/convert response is captured. We poll the captured
    list rather than relying solely on page.expect_response(), because the
    response may have already arrived by the time we get here (we attached
    the listener before submitting).
    """
    if captured:
        return captured[-1]

    try:
        page.wait_for_event(
            "response",
            predicate=lambda r: CONVERT_API_FRAGMENT in r.url and r.request.method == "POST",
            timeout=timeout_ms,
        )
    except PlaywrightTimeoutError:
        raise IGMediaError(
            f"no POST to {CONVERT_API_FRAGMENT} within {timeout_ms}ms — the page "
            "may have changed how it submits, or the request is being blocked"
        )

    # The response handler runs on the event loop; poll briefly to give it a
    # chance to populate `captured` (it normally races our return).
    page.wait_for_function(
        "() => true", timeout=500
    )  # micro-yield
    if not captured:
        # Extremely unlikely, but be defensive.
        raise IGMediaError("convert response event fired but body was not captured")
    return captured[-1]


# ─────────────────────────── response normalization ──────────────────────────
# anonyig.com's response shape is undocumented, so we accept several common
# layouts seen in their wire traffic and surface them all to the same schema
# as ig_media.py (the yt-dlp version) for drop-in compatibility.

def _normalize(body: Any, original_url: str) -> dict[str, Any]:
    """
    Flatten anonyig.com's convert response into the same schema produced by
    ig_media.py. The actual shape (verified empirically) is:

        [
          {
            "url":   [ {"url": "...", "name": "JPG", "type": "jpg", "ext": "jpg"}, ... ],
            "thumb": "...",
            "meta":  { "title": "...", "username": "...", "shortcode": "...",
                       "source": "...", "taken_at": 12345, "like_count": ...,
                       "comment_count": ..., "comments": [...] }
          },
          ... one entry per item in the carousel ...
        ]

    Each `url` is itself an array of variant qualities; we take the first
    (anonyig orders them best-to-worst). `meta` is duplicated across every
    item, so we read post-level fields from the first item and ignore the
    rest. We deliberately drop the `comments` array — the caller asked for
    media URLs, not the comment thread.
    """
    if isinstance(body, dict):
        # Defensive: if anonyig ever wraps the array in an envelope.
        body = body.get("url_list") or body.get("data") or body.get("result") or [body]

    if not isinstance(body, list) or not body:
        raise IGMediaError(f"unexpected response shape: {type(body).__name__}")

    media: list[dict[str, Any]] = []
    for item in body:
        if not isinstance(item, dict):
            continue
        entry = _media_entry_from_item(item)
        if entry["url"]:
            media.append(entry)

    # Post-level metadata lives in `meta`, identical on every item.
    meta = (body[0] or {}).get("meta") or {}
    return {
        "post_url": meta.get("source") or original_url,
        "post_id": meta.get("shortcode"),
        "uploader": meta.get("username"),
        "caption": meta.get("title"),
        "media": media,
        "_raw": body,  # full response if --include-raw
    }


def _media_entry_from_item(item: dict[str, Any]) -> dict[str, Any]:
    """
    Extract one carousel item.

    `item['url']` is an array of quality variants; we take the first because
    anonyig.com orders them best-quality-first. If a future response changes
    that ordering, prefer the largest mp4 over jpg, etc.
    """
    variants = item.get("url") or []
    if not isinstance(variants, list):
        variants = [variants] if variants else []
    primary = variants[0] if variants else {}
    if not isinstance(primary, dict):
        # Some endpoints inline the URL as a bare string.
        primary = {"url": primary} if primary else {}

    ext = (primary.get("ext") or primary.get("type") or "").lower()
    is_video = ext in {"mp4", "mov", "m3u8", "webm"} or "video" in ext

    return {
        "type": "video" if is_video else "image",
        "url": primary.get("url"),
        "ext": ext or None,
        "thumbnail": item.get("thumb"),
    }


# ─────────────────────────── CLI ─────────────────────────────────────────────

def _cli() -> int:
    parser = argparse.ArgumentParser(
        description="Extract Instagram media URLs by driving anonyig.com via Playwright."
    )
    parser.add_argument("url", help="Instagram post / reel / IGTV URL")
    parser.add_argument("--headed", action="store_true",
                        help="Show the browser window (default: headless)")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_MS,
                        help=f"Overall timeout in ms (default {DEFAULT_TIMEOUT_MS})")
    parser.add_argument("--pretty", action="store_true",
                        help="Pretty-print JSON output")
    parser.add_argument("--debug", action="store_true",
                        help="Print step-by-step progress to stderr")
    parser.add_argument("--include-raw", action="store_true",
                        help="Include the raw upstream response in the output")
    args = parser.parse_args()

    try:
        data = extract_media(
            args.url,
            headed=args.headed,
            timeout_ms=args.timeout,
            debug=args.debug,
        )
    except IGMediaError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    if not args.include_raw:
        data.pop("_raw", None)

    print(json.dumps(data, indent=2 if args.pretty else None, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())