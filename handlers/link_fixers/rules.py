"""Hardcoded link fixer rules."""

from dataclasses import dataclass


@dataclass(frozen=True)
class LinkFixerRule:
    """Configuration for a link fixer service."""

    name: str
    pattern: str
    replacement: str
    description: str = ""


LINK_FIXERS = [
    LinkFixerRule(
        name="x",
        pattern=r"https?://(?:www\.)?(?:x|twitter)\.com",
        replacement="https://fixupx.com",
        description="X/Twitter -> fixupx.com",
    ),
    LinkFixerRule(
        name="tiktok",
        pattern=r"https?://(?:www\.|vt\.)?tiktok\.com",
        replacement="https://www.tfxktok.com",
        description="TikTok -> tfxktok.com",
    ),
    LinkFixerRule(
        name="youtube",
        pattern=r"https?://(?:www\.|m\.)?(?:youtube\.com|youtu\.be)",
        replacement="https://koutube.com",
        description="YouTube -> koutube.com",
    ),
    LinkFixerRule(
        name="pixiv",
        pattern=r"https?://(?:www\.)?pixiv\.net",
        replacement="https://phixiv.net",
        description="Pixiv -> phixiv.net",
    ),
]
