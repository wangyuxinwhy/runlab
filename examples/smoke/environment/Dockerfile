FROM python:3.14-slim

WORKDIR /workspace
ENTRYPOINT ["python", "-c", "import pathlib,sys; prompt=sys.stdin.read(); pathlib.Path('/artifacts/answer.txt').write_text(prompt.upper()); print('done')"]
