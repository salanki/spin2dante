FROM python:3.13-alpine
RUN apk add --no-cache ffmpeg
RUN pip install --no-cache-dir sendspin
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
EXPOSE 8927
ENTRYPOINT ["/entrypoint.sh"]
