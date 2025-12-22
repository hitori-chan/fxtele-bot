FROM python:alpine

WORKDIR /app

COPY requirements.txt .

RUN pip install --no-cache-dir -r requirements.txt && \
    adduser -SDH bot && \
    mkdir cookies

COPY . .

USER bot

CMD ["python", "main.py"]

