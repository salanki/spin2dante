FROM python:3.13-alpine
RUN apk add --no-cache sox
RUN pip install --no-cache-dir netaudio
COPY monitor.sh /monitor.sh
RUN chmod +x /monitor.sh
ENTRYPOINT ["/monitor.sh"]
