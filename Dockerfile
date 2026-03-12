FROM python:3.12-alpine

RUN apk add --no-cache git openssh-client

RUN pip install --no-cache-dir websockets boto3

RUN adduser -u 1001 -D agent && mkdir -p /workspace && chown 1001:1001 /workspace

COPY agent-bridge.py /app/agent-bridge.py

WORKDIR /workspace

ENV APP_KIND=cloud \
    WORKSPACE_DIR=/workspace \
    PYTHONUNBUFFERED=1

USER agent
CMD ["python3", "/app/agent-bridge.py"]
