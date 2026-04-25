"""Instagram media extractor using AnonyIG logic."""

import asyncio
import logging
import re
from typing import Any

from core.registry import register_handler
from core.types import HandlerResult, HandlerType
from .base import MediaExtractor
from .anonyig import extract_media, IGMediaError

logger = logging.getLogger(__name__)

RE_INSTAGRAM = re.compile(r"(https?://(?:www\.)?instagram\.com/\S+)")


@register_handler("instagram")
class InstagramExtractor(MediaExtractor):
    """Extract media from Instagram via AnonyIG Playwright script."""

    name = "instagram"
    url_pattern = RE_INSTAGRAM

    async def _extract_media(self, url: str) -> HandlerResult | None:
        """Use the playwright-based extractor to get direct links."""
        try:
            # Run the synchronous Playwright extraction in a thread
            data = await asyncio.to_thread(extract_media, url)
            
            if not data or not data.get("media"):
                logger.warning(f"AnonyIG returned no media for {url}")
                return self._fallback_result(url)

            # Construct media URL list starting with source for context
            media_urls = [url]
            
            for item in data.get("media", []):
                m_url = item.get("url")
                if not m_url:
                    continue
                
                # Append extension hints to ensure the router detects type correctly
                m_type = item.get("type", "image")
                ext = ".mp4" if m_type == "video" else ".jpg"
                
                if ext not in m_url.lower():
                    m_url += ("&" if "?" in m_url else "?") + ext
                
                media_urls.append(m_url)

            # De-duplicate while preserving order
            unique_urls = list(dict.fromkeys(media_urls))

            return HandlerResult(
                type=HandlerType.MEDIA_EXTRACTOR,
                content=unique_urls,
                metadata={
                    "original_url": url,
                    "caption": data.get("caption"),
                    "uploader": data.get("uploader"),
                    "post_id": data.get("post_id")
                },
            )

        except IGMediaError as e:
            logger.error(f"AnonyIG extraction error for {url}: {e}")
            return self._fallback_result(url)
        except Exception as e:
            logger.error(f"Unexpected error using AnonyIG for {url}: {e}")
            return self._fallback_result(url)

    def _fallback_result(self, url: str) -> HandlerResult:
        """Return a basic link fixer result as fallback."""
        vx_url = url.replace("instagram.com", "vxinstagram.com")
        return HandlerResult(
            type=HandlerType.LINK_FIXER,
            content=vx_url,
        )
