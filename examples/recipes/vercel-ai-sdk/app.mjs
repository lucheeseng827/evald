// Vercel AI SDK -> evald: a tool-calling generateText, on the SDK's OpenTelemetry integration.
//
// The integration is the first block: an OpenTelemetry NodeSDK exporting OTLP to evald, and the
// AI SDK's `@ai-sdk/otel` integration registered once. Run it with
// examples/recipes/run.sh vercel-ai-sdk, or against your own evald:
//
//   OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 OTEL_SERVICE_NAME=my-app node app.mjs
import { OTLPTraceExporter } from '@opentelemetry/exporter-trace-otlp-proto';
import { NodeSDK } from '@opentelemetry/sdk-node';
import { OpenTelemetry } from '@ai-sdk/otel';
import { generateText, registerTelemetry, stepCountIs, tool } from 'ai';

// service.name from OTEL_SERVICE_NAME, the endpoint from OTEL_EXPORTER_OTLP_ENDPOINT
const sdk = new NodeSDK({ traceExporter: new OTLPTraceExporter() });
sdk.start();
registerTelemetry(new OpenTelemetry());

const { createOpenAI } = await import('@ai-sdk/openai');
const { z } = await import('zod');

const openai = createOpenAI({ baseURL: process.env.OPENAI_BASE_URL });

const result = await generateText({
  model: openai.chat('gpt-4o-mini'), // Chat Completions; `openai('...')` is the Responses API
  tools: {
    get_weather: tool({
      description: 'The current weather in a city.',
      inputSchema: z.object({ city: z.string() }),
      execute: async ({ city }) => `sunny, 21 degrees in ${city}`,
    }),
  },
  stopWhen: stepCountIs(3),
  prompt: 'What is the weather in Paris?',
  telemetry: { functionId: 'weather' },
});
console.log(result.text);
await sdk.shutdown(); // flush the last spans before the process exits
