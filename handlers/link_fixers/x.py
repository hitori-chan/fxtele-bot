"""X (Twitter) link fixer."""

import re

from core.registry import register_handler
from .base import LinkFixer


@register_handler("x")
class XFixer(LinkFixer):
    """Replace X/Twitter URLs with fixupx.com."""

    name = "x"
    pattern = re.compile(r"https?://(?:www\.)?(?:x|twitter)\.com")
    replacement = "https://fixupx.com"
