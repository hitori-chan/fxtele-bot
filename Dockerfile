FROM python:3.12-slim

WORKDIR /app
ENV PYTHONDONTWRITEBYTECODE=1
ENV PYTHONUNBUFFERED=1
ENV PLAYWRIGHT_BROWSERS_PATH=/ms-playwright

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
RUN playwright install --with-deps chromium && chmod -R 755 /ms-playwright

RUN useradd --create-home --shell /usr/sbin/nologin bot && mkdir -p /app/data && chown -R bot:bot /app
USER bot

COPY --chown=bot:bot . .

CMD ["python", "main.py"]
