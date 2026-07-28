FROM python:3.13-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends sox && \
    rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir netaudio
COPY monitor.sh /monitor.sh
RUN chmod +x /monitor.sh
ENTRYPOINT ["/monitor.sh"]
