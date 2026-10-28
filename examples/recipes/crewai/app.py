"""CrewAI -> evald: a one-agent crew with one tool, traced with OpenInference.

The integration is the first block: a tracer provider exporting OTLP to evald, the CrewAI
instrumentor (crews, agents, tasks, tools) and the OpenAI instrumentor (the model calls, which
CrewAI makes through the OpenAI SDK). CREWAI_DISABLE_TELEMETRY=true keeps CrewAI's own usage
telemetry off; run.sh sets it. Run it with examples/recipes/run.sh crewai, or against your evald:

    CREWAI_DISABLE_TELEMETRY=true OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 \\
        OTEL_SERVICE_NAME=my-crew python app.py
"""

import os

from openinference.instrumentation.crewai import CrewAIInstrumentor
from openinference.instrumentation.openai import OpenAIInstrumentor
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

provider = TracerProvider()  # service.name from OTEL_SERVICE_NAME
provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))  # OTEL_EXPORTER_OTLP_ENDPOINT
CrewAIInstrumentor().instrument(tracer_provider=provider)
OpenAIInstrumentor().instrument(tracer_provider=provider)

from crewai import LLM, Agent, Crew, Task  # noqa: E402
from crewai.tools import tool  # noqa: E402


@tool("get_weather")
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"sunny, 21 degrees in {city}"


llm = LLM(model="gpt-4o-mini", base_url=os.environ.get("OPENAI_BASE_URL"))
forecaster = Agent(
    role="Forecaster",
    goal="Answer weather questions",
    backstory="You look the weather up with your tool.",
    tools=[get_weather],
    llm=llm,
)
task = Task(description="What is the weather in Paris?", expected_output="One sentence.", agent=forecaster)
print(Crew(agents=[forecaster], tasks=[task]).kickoff())
provider.shutdown()  # flush the last spans before the process exits
