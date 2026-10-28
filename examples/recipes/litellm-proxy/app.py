"""LiteLLM proxy -> evald: run the proxy with its OpenTelemetry callback, call it, check evald.

The integration is config.yaml (`callbacks: ["otel"]`) plus two environment variables naming
the exporter and evald's endpoint. This script starts the proxy, sends one chat completion
through it with the OpenAI client, and stops it. Run it with examples/recipes/run.sh
litellm-proxy, or start the proxy yourself:

    LITELLM_MASTER_KEY=sk-... OTEL_EXPORTER=otlp_http OTEL_ENDPOINT=http://127.0.0.1:4318/v1/traces \\
        litellm --config config.yaml --port 4000
"""

import os
import secrets
import signal
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from openai import OpenAI

PORT = int(os.environ.get("LITELLM_PORT", "14000"))
evald = os.environ["OTEL_EXPORTER_OTLP_ENDPOINT"].rstrip("/")
master_key = "sk-" + secrets.token_hex(16)  # the proxy refuses to start unauthenticated
env = dict(
    os.environ,
    LITELLM_MASTER_KEY=master_key,
    OTEL_EXPORTER="otlp_http",
    OTEL_ENDPOINT=f"{evald}/v1/traces",
    OTEL_BSP_SCHEDULE_DELAY="200",  # export within a fraction of a second
    LITELLM_TELEMETRY="False",
    LITELLM_LOCAL_MODEL_COST_MAP="True",  # the bundled price map: no fetch at startup
)
config = Path(__file__).with_name("config.yaml")
proxy = subprocess.Popen(
    # the `litellm` entry point installed next to this interpreter
    [str(Path(sys.executable).with_name("litellm")), "--config", str(config), "--host", "127.0.0.1", "--port", str(PORT)],
    env=env,
)
try:
    for _ in range(600):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health/liveliness", timeout=1)
            break
        except Exception:
            if proxy.poll() is not None:
                sys.exit("the proxy exited during startup")
            time.sleep(0.1)
    client = OpenAI(base_url=f"http://127.0.0.1:{PORT}", api_key=master_key)
    reply = client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "What is the weather in Paris?"}],
        user="recipe-user",
    )
    print(reply.choices[0].message.content)
    time.sleep(2)  # the callback exports in the background
finally:
    proxy.send_signal(signal.SIGINT)
    try:
        proxy.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proxy.kill()
