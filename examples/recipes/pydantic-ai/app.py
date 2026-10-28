"""Pydantic AI -> evald: an agent with one tool, on Pydantic AI's built-in OpenTelemetry.

Pydantic AI emits the OpenTelemetry GenAI conventions itself; no instrumentation package is
needed, only a tracer provider that exports OTLP to evald (the first block). Run it with
examples/recipes/run.sh pydantic-ai, or against your own evald:

    OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 OTEL_SERVICE_NAME=my-agent python app.py
"""

import os

from opentelemetry import trace
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

provider = TracerProvider()  # service.name from OTEL_SERVICE_NAME
provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))  # OTEL_EXPORTER_OTLP_ENDPOINT
trace.set_tracer_provider(provider)

from pydantic_ai import Agent  # noqa: E402
from pydantic_ai.models.openai import OpenAIChatModel  # noqa: E402
from pydantic_ai.providers.openai import OpenAIProvider  # noqa: E402

Agent.instrument_all()  # every agent reports to the global tracer provider

model = OpenAIChatModel("gpt-4o-mini", provider=OpenAIProvider(base_url=os.environ.get("OPENAI_BASE_URL")))
agent = Agent(model, name="weather-agent", instructions="Answer weather questions with get_weather.")


@agent.tool_plain
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"sunny, 21 degrees in {city}"


result = agent.run_sync("What is the weather in Paris?")
print(result.output)
provider.shutdown()  # flush the last spans before the process exits
