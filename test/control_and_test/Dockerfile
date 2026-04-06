FROM python:3.13-alpine
RUN apk add --no-cache sox build-base git linux-headers
RUN pip install --no-cache-dir netaudio
COPY control_and_test.sh /control_and_test.sh
RUN chmod +x /control_and_test.sh
ENTRYPOINT ["/control_and_test.sh"]
