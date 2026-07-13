// The OSS view registry — the console is data-driven from this list. Each entry
// yields its nav item, page header, endpoint tags, and route.
//
// This file is OSS (Apache-2.0) and ships in the OSS binary + public mirror, so it
// contains ONLY the Local node · OSS surfaces. The EE fleet surfaces live in the
// private `ee/` tree (ee/frontend), which imports this `ViewDef` type + these OSS
// views and appends its own — see ee/frontend/src/registry.tsx.
import type { ComponentType } from "react";
import { Overview } from "./views/Overview";
import { Traces } from "./views/Traces";
import { Evals } from "./views/Evals";
import { Scores } from "./views/Scores";
import { Sql } from "./views/Sql";
import { Cost } from "./views/Cost";

// Closed on purpose: a typo'd group here silently drops a nav item into no
// section rather than failing to compile, and the EE tree appends "ee" (never
// this OSS one) to the very same registry, so both editions' groups belong here.
export type ViewGroup = "oss" | "ee";

export interface ViewDef {
  key: string;
  group: ViewGroup;
  label: string;
  icon: string;
  title: string;
  sub: string;
  ep: string[];
  Component: ComponentType;
}

export const GROUP_LABEL: Record<string, string> = {
  oss: "Local node · OSS",
};

export const VIEWS: ViewDef[] = [
  { key: "overview", group: "oss", label: "Overview", icon: "gauge", title: "Overview", sub: "Local node · OTLP/HTTP receiver + durable store", ep: ["GET /v1/stats"], Component: Overview },
  { key: "traces", group: "oss", label: "Traces", icon: "list", title: "Traces", sub: "Trace list → span tree → scores · hot ∪ cold, deduped", ep: ["GET /v1/spans", "GET /v1/traces/{id}"], Component: Traces },
  { key: "evals", group: "oss", label: "Evals", icon: "check", title: "Evals", sub: "Offline regression loop · deterministic Tier-1 + LLM-judge", ep: ["evald eval run", "GET /v1/scores"], Component: Evals },
  { key: "scores", group: "oss", label: "Scores", icon: "spark", title: "Scores", sub: "Universal score object · eval / human / api", ep: ["GET /v1/scores"], Component: Scores },
  { key: "sql", group: "oss", label: "SQL console", icon: "file", title: "SQL console", sub: "Read-only DataFusion over Parquet blocks ∪ hot tier", ep: ["POST /v1/sql"], Component: Sql },
  { key: "cost", group: "oss", label: "Cost", icon: "activity", title: "Cost", sub: "Token + spend attribution across the store", ep: ["POST /v1/sql"], Component: Cost },
];
