"""Pixiv link fixer."""

import re

from core.registry import register_handler
from .base import LinkFixer


@register_handler("pixiv_url")
class PixivFixer(LinkFixer):
    """Replace Pixiv URLs with phixiv.net."""

    name = "pixiv_url"
    pattern = re.compile(r"https?://(?:www\.)?pixiv\.net")
    replacement = "https://phixiv.net"
