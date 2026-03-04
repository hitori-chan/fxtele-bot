"""YouTube link fixer."""

import re

from core.registry import register_handler
from .base import LinkFixer


@register_handler("youtube")
class YouTubeFixer(LinkFixer):
    """Replace YouTube URLs with koutube.com."""

    name = "youtube"
    pattern = re.compile(r"https?://(?:www\.|m\.)?(?:youtube\.com|youtu\.be)")
    replacement = "https://koutube.com"
