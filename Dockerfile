FROM python:alpine

WORKDIR /app

COPY requirements.txt .

RUN pip install --no-cache-dir -r requirements.txt && \
    adduser -SDH bot

COPY bot.py .

USER bot

CMD ["python", "bot.py"]

