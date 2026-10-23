# Node.js runs services/anonyig_signer/signer.js, which drives a jsdom copy of
# anonyig.com to sign Instagram requests using its own client-side JS. jsdom
# needs a newer Node than Debian packages, so Node comes from the official image.
# The signer is launched with Node's --allow-net permission flag, which needs Node 26+.
FROM node:26-slim AS signer
WORKDIR /signer
COPY services/anonyig_signer/package.json services/anonyig_signer/package-lock.json ./
RUN npm ci --omit=dev

# Static ffmpeg/ffprobe re-encode videos that exceed Telegram's upload limit.
# Debian's ffmpeg package adds ~440 MB of codec and graphics libraries instead.
FROM mwader/static-ffmpeg:9.0.2-1 AS ffmpeg

FROM python:3.15-slim

WORKDIR /app
ENV PYTHONDONTWRITEBYTECODE=1
ENV PYTHONUNBUFFERED=1

# The official Node binary links against libatomic, which the Python slim image lacks.
RUN apt-get update && apt-get install -y --no-install-recommends libatomic1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=signer /usr/local/bin/node /usr/local/bin/node
COPY --from=ffmpeg /ffmpeg /ffprobe /usr/local/bin/

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY --from=signer /signer/node_modules services/anonyig_signer/node_modules

RUN useradd --create-home --shell /usr/sbin/nologin bot && mkdir -p /app/data/cookies && chown -R bot:bot /app
USER bot

COPY --chown=bot:bot . .

CMD ["python", "main.py"]
