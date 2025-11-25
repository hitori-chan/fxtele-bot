FROM python:alpine

RUN pip install --no-cache-dir python-telegram-bot

WORKDIR /app

COPY bot.py .

CMD ["python", "bot.py"]

