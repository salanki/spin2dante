FROM python:3.13-slim
RUN pip install --no-cache-dir netaudio
COPY control_and_test_multi/control_and_test_multi.sh /control_and_test_multi.sh
COPY common/audio_compare.py /audio_compare.py
COPY common/audio_artifact_analyzer.py /audio_artifact_analyzer.py
RUN chmod +x /control_and_test_multi.sh
ENTRYPOINT ["/control_and_test_multi.sh"]
