"""OpenAI Agents SDK -> evald: an agent with one function tool, traced with OpenInference.

The integration is the first block. `exclusive_processor=True` replaces the SDK's own trace
processor, which would otherwise upload every trace to OpenAI's platform: with it, spans go to
evald and nowhere else. Run it with examples/recipes/run.sh openai-agents, or against your evald:

    OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 OTEL_SERVICE_NAME=my-agent python app.py
"""

from openinference.instrumentation.openai_agents import OpenAIAgentsInstrumentor
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

provider = TracerProvider()  # service.name from OTEL_SERVICE_NAME
provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))  # OTEL_EXPORTER_OTLP_ENDPOINT
OpenAIAgentsInstrumentor().instrument(tracer_provider=provider, exclusive_processor=True)

from agents import Agent, Runner, function_tool  # noqa: E402


@function_tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"sunny, 21 degrees in {city}"


agent = Agent(
    name="weather-agent",
    instructions="Answer weather questions with the get_weather tool.",
    model="gpt-4o-mini",  # the Responses API, at OPENAI_BASE_URL
    tools=[get_weather],
)
result = Runner.run_sync(agent, "What is the weather in Paris?")
print(result.final_output)
provider.shutdown()  # flush the last spans before the process exits
