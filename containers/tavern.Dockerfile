FROM python:3.8
WORKDIR /proj
RUN pip install tavern pytest==7.0.0
ADD tavern.yaml integration.tavern.yaml
CMD py.test -vv --junit-xml=test-reports/test.xml integration.tavern.yaml
