FROM python:3.12-slim

# Set environment variables for Playwright
ENV PLAYWRIGHT_BROWSERS_PATH=/usr/local/bin/playwright-browsers
ENV PYTHONUNBUFFERED=1

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

# Install Playwright and browsers as root to a shared location
RUN playwright install chromium
RUN playwright install-deps chromium

# Create bot user and set permissions for the shared browser directory
RUN adduser --disabled-password bot && \
    mkdir -p /usr/local/bin/playwright-browsers && \
    chmod -R 755 /usr/local/bin/playwright-browsers

USER bot

COPY . .

CMD ["python", "-u", "main.py"]
