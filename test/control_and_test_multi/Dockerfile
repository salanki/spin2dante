FROM python:3.13-alpine
RUN apk add --no-cache build-base git linux-headers
RUN pip install --no-cache-dir netaudio
COPY control_and_test_multi/control_and_test_multi.sh /control_and_test_multi.sh
COPY common/audio_compare.py /audio_compare.py
RUN chmod +x /control_and_test_multi.sh
ENTRYPOINT ["/control_and_test_multi.sh"]
