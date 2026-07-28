FROM python:3.13-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends sox && \
    rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir netaudio
COPY control_and_test/control_and_test.sh /control_and_test.sh
COPY common/audio_compare.py /audio_compare.py
COPY common/audio_artifact_analyzer.py /audio_artifact_analyzer.py
RUN chmod +x /control_and_test.sh
ENTRYPOINT ["/control_and_test.sh"]
