FROM python:3.12-slim

WORKDIR /app
ENV PYTHONDONTWRITEBYTECODE=1
ENV PYTHONUNBUFFERED=1

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

RUN useradd --create-home --shell /usr/sbin/nologin bot && mkdir -p /app/data/cookies && chown -R bot:bot /app
USER bot

COPY --chown=bot:bot . .

CMD ["python", "main.py"]
