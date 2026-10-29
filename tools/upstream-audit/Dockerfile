FROM python:3.12-alpine

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    AUDIT_DATA_DIR=/data

RUN apk add --no-cache ca-certificates \
    && addgroup -g 10001 audit \
    && adduser -D -u 10001 -G audit audit \
    && mkdir -p /app /data \
    && chown 10001:10001 /app /data

WORKDIR /app

COPY --chown=10001:10001 server.py ./server.py
COPY --chown=10001:10001 static/ ./static/

USER 10001:10001

VOLUME ["/data"]
EXPOSE 8991

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["python", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8991/health', timeout=2).read()"]

CMD ["python", "server.py"]
