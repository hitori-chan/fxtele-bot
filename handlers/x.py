import re

RE_X = re.compile(r"https?://(?:www\.)?(?:x|twitter)\.com")


async def handle_x(text: str) -> dict[str, str] | None:
    """Replace X/Twitter URLs with privacy-friendly alternatives."""
    fixed = text
    fixed = RE_X.sub("https://fixupx.com", fixed)

    if fixed != text:
        return {"type": "text", "text": fixed}
    return None
