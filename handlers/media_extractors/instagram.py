"""Instagram media extractor."""

import logging
import re
from urllib.parse import urlparse

import httpx
from lxml import html

from config import USER_AGENT, HTTP_TIMEOUT
from core.registry import register_handler
from core.types import HandlerResult, HandlerType
from services.http import get_client
from .base import MediaExtractor

logger = logging.getLogger(__name__)

RE_INSTAGRAM = re.compile(r"(https?://(?:www\.)?instagram\.com/\S+)")


@register_handler("instagram")
class InstagramExtractor(MediaExtractor):
    """Extract media from Instagram via vxinstagram.com."""

    name = "instagram"
    url_pattern = RE_INSTAGRAM

    async def _extract_media(self, url: str) -> HandlerResult | None:
        """Extract media from vxinstagram version of the URL."""
        vx_url = url.replace("instagram.com", "vxinstagram.com")

        try:
            client = get_client()
            headers = {"User-Agent": USER_AGENT}

            if not client:
                async with httpx.AsyncClient(timeout=HTTP_TIMEOUT) as temp_client:
                    response = await temp_client.get(
                        vx_url, headers=headers, follow_redirects=True
                    )
            else:
                response = await client.get(
                    vx_url, headers=headers, follow_redirects=True
                )

            response.raise_for_status()

            tree = html.fromstring(response.text)
            
            # Find all links containing d.rapidcdn.app
            xpath_query = (
                '//@src[contains(., "d.rapidcdn.app")] | '
                '//@href[contains(., "d.rapidcdn.app")]'
            )
            media_elements = tree.xpath(xpath_query)

            if media_elements:
                media_urls = []
                for elem in media_elements:
                    m_url = str(elem)
                    # Handle relative URLs
                    if m_url.startswith("/"):
                        parsed_vx = urlparse(vx_url)
                        m_url = f"{parsed_vx.scheme}://{parsed_vx.netloc}{m_url}"
                    
                    # Strip dl=1 which blocks streaming/previews
                    m_url = re.sub(r'[?&]dl=1', '', m_url)
                    
                    # Detect media type for extension hint
                    # /v2 usually indicates video, /thumb usually indicates image
                    ext = ".mp4"
                    if "rapidcdn.app/thumb" in m_url:
                        ext = ".jpg"
                    
                    # Add hint extension for Telegram
                    m_url += ("&" if "?" in m_url else "?") + ext
                    
                    logger.debug("vxinstagram media: {}".format(m_url))
                    media_urls.append(m_url)

                # Remove duplicates while preserving order
                unique_urls = list(dict.fromkeys(media_urls))

                return HandlerResult(
                    type=HandlerType.MEDIA_EXTRACTOR,
                    content=unique_urls,
                    metadata={"original_url": url},
                )

            logger.warning(f"No d.rapidcdn.app links found in {vx_url}")
            # Fallback to simple link replacement
            return HandlerResult(
                type=HandlerType.LINK_FIXER,
                content=vx_url,
            )

        except Exception as e:
            logger.error(f"Error extracting Instagram media from {vx_url}: {e}")
            # Fallback to simple link replacement on error
            return HandlerResult(
                type=HandlerType.LINK_FIXER,
                content=vx_url,
            )
