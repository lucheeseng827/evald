"""LangGraph -> evald: a tool-calling agent graph, traced with OpenInference.

The integration is the first block: an OpenTelemetry tracer provider exporting OTLP to evald,
and the LangChain instrumentor (which covers LangGraph). Everything after it is ordinary
LangGraph. Run it with examples/recipes/run.sh langgraph, or against your own evald:

    OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 OTEL_SERVICE_NAME=my-agent python app.py
"""

import os

from openinference.instrumentation.langchain import LangChainInstrumentor
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

provider = TracerProvider()  # service.name from OTEL_SERVICE_NAME
provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))  # OTEL_EXPORTER_OTLP_ENDPOINT
LangChainInstrumentor().instrument(tracer_provider=provider)

from langchain_core.messages import HumanMessage  # noqa: E402
from langchain_core.tools import tool  # noqa: E402
from langchain_openai import ChatOpenAI  # noqa: E402
from langgraph.graph import START, MessagesState, StateGraph  # noqa: E402
from langgraph.prebuilt import ToolNode, tools_condition  # noqa: E402


@tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"sunny, 21 degrees in {city}"


model = ChatOpenAI(model="gpt-4o-mini", base_url=os.environ.get("OPENAI_BASE_URL")).bind_tools(
    [get_weather]
)


def agent(state: MessagesState):
    return {"messages": [model.invoke(state["messages"])]}


graph = StateGraph(MessagesState)
graph.add_node("agent", agent)
graph.add_node("tools", ToolNode([get_weather]))
graph.add_edge(START, "agent")
graph.add_conditional_edges("agent", tools_condition)
graph.add_edge("tools", "agent")
app = graph.compile()

result = app.invoke(
    {"messages": [HumanMessage("What is the weather in Paris?")]},
    config={"metadata": {"session_id": "recipe-langgraph-session"}},
)
print(result["messages"][-1].content)
provider.shutdown()  # flush the last spans before the process exits
