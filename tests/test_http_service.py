"""Regression tests for HTTP client startup logging."""

import unittest
from unittest.mock import AsyncMock, MagicMock, patch

from services import http


class ProxyLogTests(unittest.IsolatedAsyncioTestCase):
    async def test_proxy_credentials_never_reach_the_logs(self):
        client = MagicMock(aclose=AsyncMock())

        with (
            patch.object(http, "HTTP_PROXY", "socks5://user:SECRET@127.0.0.1:40000"),
            patch.object(http.httpx, "AsyncClient", return_value=client) as make_client,
            self.assertLogs("services.http", level="INFO") as logs,
        ):
            await http.init_http_client(None)
            await http.shutdown_http_client(None)

        self.assertEqual(
            logs.records[0].getMessage(),
            "Initialized global HTTP client routing through proxy socks5://127.0.0.1:40000.",
        )
        self.assertEqual(make_client.call_args.kwargs["proxy"], "socks5://user:SECRET@127.0.0.1:40000")


if __name__ == "__main__":
    unittest.main()
