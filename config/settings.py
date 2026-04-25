"""Centralized configuration settings."""

import os
from dataclasses import dataclass

# HTTP Configuration
HTTP_TIMEOUT = 10.0
USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36"
)
INLINE_CACHE_TIME = 300  # 5 minutes

# Facebook Request Headers
FACEBOOK_HEADERS = {
    "User-Agent": USER_AGENT,
    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "Accept-Language": "en-US,en;q=0.5",
    "DNT": "1",
    "Sec-GPC": "1",
    "Upgrade-Insecure-Requests": "1",
    "Sec-Fetch-Dest": "document",
    "Sec-Fetch-Mode": "navigate",
    "Sec-Fetch-Site": "none",
    "Sec-Fetch-User": "?1",
    "Priority": "u=0, i",
}

# URL Processing
FACEBOOK_PARAMS_TO_KEEP = {"story_fbid", "id", "fbid", "v"}

# Facebook Auth
FACEBOOK_EMAIL = os.getenv("FACEBOOK_EMAIL")
FACEBOOK_PASSWORD = os.getenv("FACEBOOK_PASSWORD")
FACEBOOK_TOTP_SECRET = os.getenv("FACEBOOK_TOTP_SECRET")
FACEBOOK_AUTH_STATE_PATH = os.getenv("FACEBOOK_AUTH_STATE_PATH", "/app/data/facebook_state.json")


@dataclass(frozen=True)
class LinkFixerConfig:
    """Configuration for a link fixer service."""

    name: str
    pattern: str
    replacement: str
    description: str = ""


# Link Fixer Configurations
# Centralized so services can be added/removed without code changes
LINK_FIXERS = [
    LinkFixerConfig(
        name="x",
        pattern=r"https?://(?:www\.)?(?:x|twitter)\.com",
        replacement="https://fixupx.com",
        description="X/Twitter → fixupx.com",
    ),
    LinkFixerConfig(
        name="tiktok",
        pattern=r"https?://(?:www\.|vt\.)?tiktok\.com",
        replacement="https://www.tfxktok.com",
        description="TikTok → tfxktok.com",
    ),
    LinkFixerConfig(
        name="youtube",
        pattern=r"https?://(?:www\.|m\.)?(?:youtube\.com|youtu\.be)",
        replacement="https://koutube.com",
        description="YouTube → koutube.com",
    ),
    LinkFixerConfig(
        name="pixiv",
        pattern=r"https?://(?:www\.)?pixiv\.net",
        replacement="https://phixiv.net",
        description="Pixiv → phixiv.net",
    ),
]
