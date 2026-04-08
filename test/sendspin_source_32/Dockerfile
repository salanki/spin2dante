FROM python:3.13-alpine
RUN pip install --no-cache-dir websockets
COPY sendspin_source_32/server.py /server.py
EXPOSE 8927
ENTRYPOINT ["python3", "/server.py"]
