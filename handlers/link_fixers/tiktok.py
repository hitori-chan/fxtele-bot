"""TikTok link fixer."""

import re

from core.registry import register_handler
from .base import LinkFixer


@register_handler("tiktok")
class TikTokFixer(LinkFixer):
    """Replace TikTok URLs with tfxktok.com."""

    name = "tiktok"
    pattern = re.compile(r"https?://(?:www\.|vt\.)?tiktok\.com")
    replacement = "https://www.tfxktok.com"
