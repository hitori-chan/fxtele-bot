FROM python:alpine

RUN pip install --no-cache-dir python-telegram-bot requests

WORKDIR /app

COPY bot.py .

RUN adduser -SDH bot

USER bot

CMD ["python", "bot.py"]

