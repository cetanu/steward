FROM python:3.9
RUN pip install fastapi uvicorn
ADD mock_server.py server.py
CMD ["uvicorn", "server:app", "--host=0.0.0.0", "--port=8000"]

