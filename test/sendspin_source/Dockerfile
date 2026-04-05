FROM python:3.13-alpine
RUN apk add --no-cache ffmpeg
RUN pip install --no-cache-dir sendspin
COPY sendspin_source/entrypoint.sh /entrypoint.sh
COPY common/generate_reference_signal.py /generate_reference_signal.py
RUN chmod +x /entrypoint.sh
EXPOSE 8927
ENTRYPOINT ["/entrypoint.sh"]
