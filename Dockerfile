FROM python:3.12-slim

WORKDIR /app
ENV PYTHONDONTWRITEBYTECODE=1
ENV PYTHONUNBUFFERED=1

# Node.js runs services/anonyig_signer/signer.js, which drives a jsdom copy of
# anonyig.com to sign Instagram requests using its own client-side JS.
RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
    && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY services/anonyig_signer/package.json services/anonyig_signer/package-lock.json services/anonyig_signer/
RUN npm install --prefix services/anonyig_signer --omit=dev

RUN useradd --create-home --shell /usr/sbin/nologin bot && mkdir -p /app/data/cookies && chown -R bot:bot /app
USER bot

COPY --chown=bot:bot . .

CMD ["python", "main.py"]
