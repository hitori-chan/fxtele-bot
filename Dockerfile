FROM python:3.12-alpine

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

RUN adduser -D bot
USER bot

COPY . .

CMD ["python", "-u", "main.py"]
