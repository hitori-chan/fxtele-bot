"""Playwright-backed Facebook authentication state and cookie access."""

import asyncio
import json
import logging
import os
import time
from contextlib import suppress
from pathlib import Path
from typing import Any

import httpx
import pyotp
from playwright.async_api import Browser, BrowserContext, Locator, Page, async_playwright

from config import (
    FACEBOOK_AUTH_STATE_PATH,
    FACEBOOK_EMAIL,
    FACEBOOK_PASSWORD,
    FACEBOOK_TOTP_SECRET,
    USER_AGENT,
)

logger = logging.getLogger(__name__)

_LOGIN_URL = "https://www.facebook.com/login"
_HOME_URL = "https://www.facebook.com/"
_LOGIN_TIMEOUT_MS = 45_000
_TOTP_FIELD_TIMEOUT_SECONDS = 45
_AUTH_COMPLETE_TIMEOUT_SECONDS = 90
_LOGIN_FAILURE_LIMIT = 3
_AUTH_LOCK = asyncio.Lock()
_MEMORY_LOGIN_FAILURES: dict[str, int] = {}
_TOTP_FIELD_SELECTORS = (
    'input[name="approvals_code"]',
    'input[name="checkpoint_code"]',
    'input[autocomplete="one-time-code"]',
    'input[inputmode="numeric"]',
    'input[type="tel"]',
    'input[type="text"][name*="code" i]',
    'input[type="text"][aria-label*="code" i]',
    'input[type="text"][placeholder*="code" i]',
)


def facebook_auth_available() -> bool:
    """Return true when all auth environment variables are configured."""
    return bool(FACEBOOK_EMAIL and FACEBOOK_PASSWORD and FACEBOOK_TOTP_SECRET)


async def get_facebook_cookies(force_refresh: bool = False) -> httpx.Cookies:
    """Return an httpx cookie jar from a valid Playwright storage state."""
    if not facebook_auth_available():
        logger.info("Facebook auth env is incomplete; using public extraction")
        return httpx.Cookies()

    async with _AUTH_LOCK:
        state_path = Path(FACEBOOK_AUTH_STATE_PATH)
        if not force_refresh and state_path.exists():
            state = _load_storage_state(state_path)
            if _has_session_cookies(state):
                logger.info("Reusing persisted Facebook auth state; Playwright login skipped")
                _clear_login_failures(state_path)
                return storage_state_to_cookies(state)
            logger.info("Persisted Facebook auth state has no session cookies; refreshing")
        elif force_refresh:
            logger.info("Refreshing Facebook auth state")
        else:
            logger.info("No Facebook auth state found; logging in")

        _raise_if_login_blocked(state_path)
        try:
            await _ensure_valid_state(state_path, force_refresh=force_refresh)
        except Exception:
            _record_login_failure(state_path)
            raise

        _clear_login_failures(state_path)
        return storage_state_to_cookies(_load_storage_state(state_path))


def storage_state_to_cookies(storage_state: dict[str, Any]) -> httpx.Cookies:
    """Convert Playwright storage state cookies to an httpx cookie jar."""
    jar = httpx.Cookies()
    for cookie in storage_state.get("cookies", []):
        name = cookie.get("name")
        value = cookie.get("value")
        domain = cookie.get("domain")
        path = cookie.get("path", "/")
        if isinstance(name, str) and isinstance(value, str):
            jar.set(name, value, domain=domain, path=path)
    return jar


def _has_session_cookies(storage_state: dict[str, Any]) -> bool:
    names = {cookie.get("name") for cookie in storage_state.get("cookies", [])}
    return {"c_user", "xs"}.issubset(names)


def _load_storage_state(path: Path) -> dict[str, Any]:
    try:
        with path.open("r", encoding="utf-8") as state_file:
            data = json.load(state_file)
    except (OSError, json.JSONDecodeError):
        return {"cookies": [], "origins": []}
    return data if isinstance(data, dict) else {"cookies": [], "origins": []}


def _failure_state_path(state_path: Path) -> Path:
    return state_path.with_suffix(".login_failures.json")


def _load_login_failures(state_path: Path) -> dict[str, Any]:
    memory_count = _MEMORY_LOGIN_FAILURES.get(str(state_path), 0)
    try:
        with _failure_state_path(state_path).open("r", encoding="utf-8") as failure_file:
            data = json.load(failure_file)
    except (OSError, json.JSONDecodeError):
        return {"count": memory_count}
    if not isinstance(data, dict):
        return {"count": memory_count}
    file_count = int(data.get("count") or 0)
    return {"count": max(file_count, memory_count)}


def _raise_if_login_blocked(state_path: Path) -> None:
    failures = _load_login_failures(state_path)
    count = int(failures.get("count") or 0)
    if count >= _LOGIN_FAILURE_LIMIT:
        raise RuntimeError("Facebook login disabled after 3 consecutive failures")


def _record_login_failure(state_path: Path) -> None:
    failures = _load_login_failures(state_path)
    count = int(failures.get("count") or 0) + 1
    _MEMORY_LOGIN_FAILURES[str(state_path)] = count
    if count >= _LOGIN_FAILURE_LIMIT:
        logger.warning("Facebook login failed %d times; Playwright login disabled", count)
    else:
        logger.warning("Facebook login failed %d/%d times", count, _LOGIN_FAILURE_LIMIT)

    path = _failure_state_path(state_path)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("w", encoding="utf-8") as failure_file:
            json.dump({"count": count}, failure_file)
        os.chmod(path, 0o600)
    except OSError as e:
        logger.warning("Could not persist Facebook login failure marker at %s: %s", path, e.strerror)


def _clear_login_failures(state_path: Path) -> None:
    _MEMORY_LOGIN_FAILURES.pop(str(state_path), None)
    with suppress(OSError):
        _failure_state_path(state_path).unlink()


async def _ensure_valid_state(state_path: Path, force_refresh: bool = False) -> None:
    """Reuse valid state, otherwise perform a new login and persist state."""
    state_path.parent.mkdir(parents=True, exist_ok=True)

    async with async_playwright() as playwright:
        logger.info("Launching headless Chromium for Facebook auth")
        browser = await playwright.chromium.launch(headless=True)
        try:
            if not force_refresh and state_path.exists() and await _state_is_valid(browser, state_path):
                logger.info("Existing Facebook auth state validated")
                return
            await _login_and_save_state(browser, state_path)
        finally:
            await browser.close()
            logger.info("Closed headless Chromium for Facebook auth")


async def _state_is_valid(browser: Browser, state_path: Path) -> bool:
    """Check whether a persisted state still appears authenticated."""
    context = await _new_context(browser, storage_state=str(state_path))
    try:
        page = await context.new_page()
        logger.info("Validating Facebook auth state")
        response = await page.goto(_HOME_URL, wait_until="domcontentloaded", timeout=_LOGIN_TIMEOUT_MS)
        if response and response.status >= 400:
            logger.info("Facebook auth state validation returned HTTP %s", response.status)
            return False
        valid = not _looks_like_login_url(page.url)
        logger.info("Facebook auth state validation %s", "succeeded" if valid else "requires login")
        return valid
    except Exception as e:
        logger.info("Facebook auth state validation failed: %r", e)
        return False
    finally:
        await context.close()


async def _login_and_save_state(browser: Browser, state_path: Path) -> None:
    """Log in to Facebook and persist Playwright storage state."""
    if not (FACEBOOK_EMAIL and FACEBOOK_PASSWORD and FACEBOOK_TOTP_SECRET):
        raise RuntimeError("Facebook auth environment is incomplete")

    context = await _new_context(browser)
    try:
        page = await context.new_page()
        logger.info("Starting Facebook login with headless Chromium")
        await page.goto(_LOGIN_URL, wait_until="domcontentloaded", timeout=_LOGIN_TIMEOUT_MS)
        await page.fill('input[name="email"]', FACEBOOK_EMAIL)
        await page.fill('input[name="pass"]', FACEBOOK_PASSWORD)
        if not await _click_first_visible(
            page,
            (
                'button[name="login"]',
                '[role="button"]:has-text("Log in")',
                'input[type="submit"]',
                'button[type="submit"]',
            ),
        ):
            raise RuntimeError("Facebook login submit control was not found")

        await _complete_totp_if_prompted(page, FACEBOOK_TOTP_SECRET)
        await _wait_until_authenticated(page)
        if not await _context_has_session_cookies(context):
            raise RuntimeError("Facebook login completed without session cookies")
        await context.storage_state(path=str(state_path))
        os.chmod(state_path, 0o600)
        logger.info("Facebook login succeeded; auth state saved")
    except Exception as e:
        logger.warning("Facebook login failed: %s", type(e).__name__)
        raise
    finally:
        await context.close()


async def _new_context(browser: Browser, storage_state: str | None = None) -> BrowserContext:
    kwargs: dict[str, Any] = {
        "user_agent": USER_AGENT,
        "locale": "en-US",
    }
    if storage_state:
        kwargs["storage_state"] = storage_state
    return await browser.new_context(**kwargs)


async def _complete_totp_if_prompted(page: Page, secret: str) -> bool:
    """Submit a TOTP code when a two-factor input appears."""
    field = await _wait_for_totp_field(page)
    if field is None:
        logger.info("Facebook two-factor prompt was not shown")
        return False

    logger.info("Submitting Facebook two-factor code")
    await field.fill(await _fresh_totp_code(secret))
    clicked = await _click_first_visible(
        page,
        (
            '[role="button"]:has-text("Continue")',
            "text=Continue",
            'button[type="submit"]',
            'button[name="submit[Continue]"]',
            'input[type="submit"]',
        ),
        timeout=3_000,
    )
    if not clicked:
        with suppress(Exception):
            await field.press("Enter")
    logger.info("Facebook two-factor code submitted")
    return True


async def _wait_for_totp_field(page: Page) -> Locator | None:
    """Wait for the real code field without matching the login email field."""
    deadline = time.monotonic() + _TOTP_FIELD_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if await _context_has_session_cookies(page.context):
            return None

        for selector in _TOTP_FIELD_SELECTORS:
            with suppress(Exception):
                field = page.locator(selector).first
                if await field.is_visible(timeout=300):
                    return field

        if await _looks_like_totp_prompt(page):
            with suppress(Exception):
                field = page.locator('input[type="text"]').first
                if await field.is_visible(timeout=300):
                    return field

        with suppress(Exception):
            await page.wait_for_load_state("domcontentloaded", timeout=1_000)
        await asyncio.sleep(0.5)
    return None


async def _looks_like_totp_prompt(page: Page) -> bool:
    if _looks_like_login_url(page.url) and "/two_step_verification" not in page.url.lower():
        return False
    with suppress(Exception):
        body = (await page.locator("body").inner_text(timeout=500)).lower()
        return "6-digit code" in body or "authentication app" in body or "two-factor" in body
    return False


async def _fresh_totp_code(secret: str) -> str:
    """Return a TOTP code with enough lifetime left for form submission."""
    totp = pyotp.TOTP(secret)
    remaining = totp.interval - (time.time() % totp.interval)
    if remaining < 8:
        logger.info("Waiting for fresh Facebook two-factor code window")
        await asyncio.sleep(remaining + 1)
    return totp.now()


async def _wait_until_authenticated(page: Page) -> None:
    """Wait until the login/checkpoint flow lands on an authenticated page."""
    last_stage = ""
    deadline = time.monotonic() + _AUTH_COMPLETE_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        stage = await _page_auth_stage(page)
        if stage != last_stage:
            logger.info("Facebook auth stage: %s", stage)
            last_stage = stage

        if stage == "authenticated":
            return
        if stage in {"checkpoint", "unknown"}:
            await _click_first_visible(
                page,
                (
                    '[role="button"]:has-text("Continue")',
                    "text=Continue",
                    "text=OK",
                    'button[type="submit"]',
                    'input[type="submit"]',
                ),
                timeout=1_000,
            )
        await asyncio.sleep(1)
    raise RuntimeError("Facebook login did not complete")


async def _click_first_visible(page: Page, selectors: tuple[str, ...], timeout: int = 5_000) -> bool:
    for selector in selectors:
        try:
            button = page.locator(selector).first
            await button.wait_for(state="visible", timeout=timeout)
            await button.click()
            return True
        except Exception:
            continue
    return False


def _looks_like_login_url(url: str) -> bool:
    lowered = url.lower()
    return "/login" in lowered or "/checkpoint" in lowered or "/two_step_verification" in lowered


async def _context_has_session_cookies(context: BrowserContext) -> bool:
    cookies = await context.cookies([_HOME_URL, _LOGIN_URL])
    names = {cookie.get("name") for cookie in cookies}
    return {"c_user", "xs"}.issubset(names)


async def _page_auth_stage(page: Page) -> str:
    if await _context_has_session_cookies(page.context):
        return "authenticated"

    lowered = page.url.lower()
    if "/two_step_verification" in lowered:
        return "two-factor"
    if "/checkpoint" in lowered:
        return "checkpoint"
    if "/login" in lowered:
        return "login"
    return "unknown"
