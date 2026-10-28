//! `evald connect <tool>`: the exact configuration that points a tool at this evald.
//!
//! The snippets cover only what has been checked against the tool itself, live, into a real
//! `evald serve`: Claude Code 2.1.306, Codex 0.163.0 and Gemini CLI 0.65.0
//! (`docs/INSTRUMENTATION.md`).
//!
//! **Privacy first** (`ROADMAP-0.4-0.6.md` D11): where a tool exports content by default, the
//! snippet turns it off, even when that costs a report a detail, and says what it costs. Codex's
//! tool output is kept out, and with it the exit code of a command that failed. A token is never printed: when auth is on, the snippet reads it
//! from an environment variable the user sets themselves, or, for a config file that cannot
//! expand one, leaves a placeholder.

use std::fmt::Write as _;

/// The tools `evald connect` knows.
pub const TOOLS: [&str; 5] = [
    "claude-code",
    "codex",
    "gemini-cli",
    "otel-app",
    "collector",
];

/// Where the tool should send, and how.
#[derive(Debug, Clone)]
pub struct Target {
    /// Host the tool reaches evald on (`127.0.0.1`, or a name when evald is remote).
    pub host: String,
    pub http_port: u16,
    pub grpc_port: u16,
    /// gRPC (`:4317`) rather than HTTP/protobuf (`:4318`), for tools that speak both.
    pub grpc: bool,
    /// Also send the tool's spans (Claude Code: its beta span tree).
    pub traces: bool,
    /// Add the bearer header, reading the token from this environment variable.
    pub token_env: Option<String>,
    /// Claude Code and Gemini CLI: a `settings.json` block instead of shell exports.
    pub settings_json: bool,
}

impl Target {
    /// A target from the addresses `evald serve` was given. A wildcard bind is not an address
    /// a client can dial, so `0.0.0.0` / `[::]` become `127.0.0.1`. An empty gRPC address
    /// means gRPC is off, as it does for `serve`, and the snippet then uses HTTP.
    pub fn from_addrs(http: &str, grpc: &str) -> anyhow::Result<Target> {
        let parse = |a: &str, what: &str| -> anyhow::Result<std::net::SocketAddr> {
            a.parse()
                .map_err(|e| anyhow::anyhow!("invalid {what} address {a:?}: {e}"))
        };
        let h = parse(http, "--otlp-http")?;
        let g = match grpc.trim() {
            "" => None,
            a => Some(parse(a, "--otlp-grpc")?),
        };
        let host = if h.ip().is_unspecified() {
            "127.0.0.1".to_string()
        } else {
            match h.ip() {
                std::net::IpAddr::V6(v6) => format!("[{v6}]"),
                ip => ip.to_string(),
            }
        };
        Ok(Target {
            host,
            http_port: h.port(),
            grpc_port: g.map_or(0, |g| g.port()),
            grpc: g.is_some(),
            traces: false,
            token_env: None,
            settings_json: false,
        })
    }

    fn http_endpoint(&self) -> String {
        format!("http://{}:{}", self.host, self.http_port)
    }

    fn grpc_endpoint(&self) -> String {
        format!("http://{}:{}", self.host, self.grpc_port)
    }
}

/// The snippet for `tool`, or an error naming the tools there are.
pub fn snippet(tool: &str, t: &Target) -> anyhow::Result<String> {
    match tool {
        "claude-code" => Ok(claude_code(t)),
        "codex" => Ok(codex(t)),
        "gemini-cli" | "gemini" => Ok(gemini_cli(t)),
        "otel-app" => Ok(otel_app(t)),
        "collector" => Ok(collector(t)),
        other => anyhow::bail!(
            "no connect recipe for {other:?}. Known: {}. Any tool that exports OTLP can use \
             otel-app.",
            TOOLS.join(", ")
        ),
    }
}

fn claude_code(t: &Target) -> String {
    let (protocol, endpoint) = if t.grpc {
        ("grpc", t.grpc_endpoint())
    } else {
        ("http/protobuf", t.http_endpoint())
    };
    let mut vars: Vec<(&str, String)> = vec![
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1".into()),
        ("OTEL_LOGS_EXPORTER", "otlp".into()),
        // evald does not ingest OTLP metrics; the events carry what it needs.
        ("OTEL_METRICS_EXPORTER", "none".into()),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", protocol.into()),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        // Logs more often than traces, so a call's event (which carries Claude Code's own
        // cost) usually reaches evald before its span.
        ("OTEL_LOGS_EXPORT_INTERVAL", "1000".into()),
    ];
    if t.traces {
        vars.push(("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1".into()));
        vars.push(("OTEL_TRACES_EXPORTER", "otlp".into()));
    }
    let header = t
        .token_env
        .as_ref()
        .map(|env| format!("Authorization=Bearer ${{{env}}}"));

    let mut out = String::new();
    if t.settings_json {
        let _ = writeln!(
            out,
            "// Claude Code -> evald. Merge into ~/.claude/settings.json (every session) or a \
             project's .claude/settings.json."
        );
        if t.token_env.is_some() {
            let _ = writeln!(
                out,
                "// settings.json does not expand variables: put the token itself in \
                 OTEL_EXPORTER_OTLP_HEADERS, or export that one variable in your shell instead."
            );
        }
        let mut env = serde_json::Map::new();
        for (k, v) in &vars {
            env.insert((*k).into(), serde_json::Value::String(v.clone()));
        }
        if t.token_env.is_some() {
            env.insert(
                "OTEL_EXPORTER_OTLP_HEADERS".into(),
                serde_json::Value::String("Authorization=Bearer <your evald token>".into()),
            );
        }
        let body = serde_json::json!({ "env": env });
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&body).expect("a JSON object")
        );
        return out;
    }
    let _ = writeln!(
        out,
        "# Claude Code -> evald: every API call (model, tokens, Claude Code's own cost), tool\n\
         # call and turn, from Claude Code's events. Checked against Claude Code 2.1.306."
    );
    for (k, v) in &vars {
        let _ = writeln!(out, "export {k}={v}");
    }
    if let Some(h) = header {
        let _ = writeln!(out, "export OTEL_EXPORTER_OTLP_HEADERS=\"{h}\"");
    }
    if !t.traces {
        let _ = writeln!(
            out,
            "# Optional, the span tree of each turn (beta; `evald connect claude-code --traces`):\n\
             # export CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1\n\
             # export OTEL_TRACES_EXPORTER=otlp"
        );
    }
    // The other direction: Claude Code asking evald (its MCP endpoint, on the HTTP port).
    let auth = t
        .token_env
        .as_ref()
        .map(|env| format!(" --header \"Authorization: Bearer ${{{env}}}\""))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "# Optional, let Claude Code query evald (sessions, cost, SQL; read-only), once:\n\
         # claude mcp add --transport http evald {}/mcp{auth}",
        t.http_endpoint()
    );
    out
}

/// Codex reads `~/.codex/config.toml`, which expands no variables: a token goes in as written.
fn codex(t: &Target) -> String {
    let exporter = |signal: &str| -> String {
        let headers = if t.token_env.is_some() {
            ", headers = { authorization = \"Bearer <your evald token>\" }"
        } else {
            ""
        };
        if t.grpc {
            format!(
                "{{ otlp-grpc = {{ endpoint = \"{}\"{headers} }} }}",
                t.grpc_endpoint()
            )
        } else {
            format!(
                "{{ otlp-http = {{ endpoint = \"{}/v1/{signal}\", protocol = \"binary\"{headers} }} }}",
                t.http_endpoint()
            )
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Codex CLI -> evald: every model call (model, tokens), tool call and turn, from\n\
         # Codex's events. Checked against Codex 0.163.0. Merge into ~/.codex/config.toml."
    );
    if let Some(env) = &t.token_env {
        let _ = writeln!(
            out,
            "# config.toml does not expand variables: replace the placeholder with the token\n\
             # (the value of ${env}), and keep the file private."
        );
    }
    let _ = writeln!(out, "[otel]");
    let _ = writeln!(out, "exporter = {}", exporter("logs"));
    let _ = writeln!(
        out,
        "# Prompts stay [REDACTED] (Codex's default, stated). Codex also exports up to 2 KiB\n\
         # of every tool call's output; 0 keeps it out. The cost: a command that exits non-zero\n\
         # is a success to Codex, and its exit code is in that output, so evald cannot count it\n\
         # as a failed tool call. The commands themselves are always exported.\n\
         log_user_prompt = false\n\
         tool_result = {{ max_bytes = 0 }}"
    );
    if t.traces {
        let _ = writeln!(
            out,
            "# Codex's spans: its internals, hundreds a turn, kept but never counted as calls.\n\
             trace_exporter = {}",
            exporter("traces")
        );
    } else {
        let _ = writeln!(
            out,
            "# Optional, Codex's spans (its internals, hundreds a turn; `--traces`):\n\
             # trace_exporter = {}",
            exporter("traces")
        );
    }
    let _ = writeln!(
        out,
        "# Codex sends its metrics to OpenAI's analytics unless told otherwise; evald does not\n\
         # take metrics. To stop them: metrics_exporter = \"none\""
    );
    out
}

/// Gemini CLI reads `GEMINI_TELEMETRY_*` variables, or `~/.gemini/settings.json`.
fn gemini_cli(t: &Target) -> String {
    let (protocol, endpoint) = if t.grpc {
        ("grpc", t.grpc_endpoint())
    } else {
        ("http", t.http_endpoint())
    };
    let mut out = String::new();
    if t.settings_json {
        let _ = writeln!(
            out,
            "// Gemini CLI -> evald. Merge into ~/.gemini/settings.json (or a project's\n\
             // .gemini/settings.json). Checked against Gemini CLI 0.65.0. Gemini CLI logs\n\
             // prompts unless logPrompts is false."
        );
        if !t.traces {
            let _ = writeln!(
                out,
                "// Its spans repeat the events, with its system prompt on each; settings.json\n\
                 // cannot turn them off, so also export OTEL_TRACES_SAMPLER=always_off."
            );
        }
        let mut telemetry = serde_json::json!({
            "enabled": true,
            "target": "local",
            "otlpProtocol": protocol,
            "otlpEndpoint": endpoint,
            "logPrompts": false,
        });
        if t.token_env.is_some() {
            let _ = writeln!(
                out,
                "// settings.json does not expand variables: put the token itself in otlpHeaders,\n\
                 // or export GEMINI_TELEMETRY_OTLP_HEADERS in your shell instead."
            );
            telemetry["otlpHeaders"] =
                serde_json::json!({ "Authorization": "Bearer <your evald token>" });
        }
        let body = serde_json::json!({ "telemetry": telemetry });
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&body).expect("a JSON object")
        );
        return out;
    }
    let _ = writeln!(
        out,
        "# Gemini CLI -> evald: every model call (model, tokens, thinking tokens), tool call and\n\
         # turn, from Gemini CLI's events. Checked against Gemini CLI 0.65.0."
    );
    let _ = writeln!(out, "export GEMINI_TELEMETRY_ENABLED=true");
    let _ = writeln!(out, "export GEMINI_TELEMETRY_TARGET=local");
    let _ = writeln!(out, "export GEMINI_TELEMETRY_OTLP_PROTOCOL={protocol}");
    let _ = writeln!(out, "export GEMINI_TELEMETRY_OTLP_ENDPOINT={endpoint}");
    let _ = writeln!(
        out,
        "# Gemini CLI logs every prompt unless told not to:\n\
         export GEMINI_TELEMETRY_LOG_PROMPTS=false"
    );
    if let Some(env) = &t.token_env {
        let _ = writeln!(
            out,
            "export GEMINI_TELEMETRY_OTLP_HEADERS=\"Authorization=Bearer ${{{env}}}\""
        );
    }
    if !t.traces {
        let _ = writeln!(
            out,
            "# Gemini CLI also sends spans of the same calls (evald never counts them twice),\n\
             # each with its 31 KB system prompt and tool list. Not sending them saves that\n\
             # (`evald connect gemini-cli --traces` keeps them):\n\
             export OTEL_TRACES_SAMPLER=always_off"
        );
    }
    let _ = writeln!(
        out,
        "# Leave GEMINI_TELEMETRY_TRACES_ENABLED unset: it adds prompts and tool output to the spans."
    );
    out
}

fn otel_app(t: &Target) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Any OpenTelemetry-instrumented app -> evald (OpenInference, OpenLLMetry, GenAI\n\
         # conventions). Set OTEL_SERVICE_NAME to your app's name."
    );
    if t.grpc {
        let _ = writeln!(out, "export OTEL_EXPORTER_OTLP_PROTOCOL=grpc");
        let _ = writeln!(
            out,
            "export OTEL_EXPORTER_OTLP_ENDPOINT={}",
            t.grpc_endpoint()
        );
    } else {
        let _ = writeln!(out, "export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf");
        let _ = writeln!(
            out,
            "export OTEL_EXPORTER_OTLP_ENDPOINT={}",
            t.http_endpoint()
        );
    }
    let _ = writeln!(out, "export OTEL_SERVICE_NAME=my-app");
    if let Some(env) = &t.token_env {
        let _ = writeln!(
            out,
            "export OTEL_EXPORTER_OTLP_HEADERS=\"Authorization=Bearer ${{{env}}}\""
        );
    }
    out
}

fn collector(t: &Target) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# OpenTelemetry Collector: send to evald beside your current backend (fan-out), so\n\
         # you can try evald without changing a single app. Add the exporter, then list it in\n\
         # the traces and logs pipelines next to the exporter you already have."
    );
    let _ = writeln!(out, "exporters:");
    if t.grpc {
        let _ = writeln!(out, "  otlp/evald:");
        let _ = writeln!(out, "    endpoint: {}:{}", t.host, t.grpc_port);
        let _ = writeln!(out, "    tls:");
        let _ = writeln!(
            out,
            "      insecure: true   # evald speaks plaintext; terminate TLS in front of it"
        );
    } else {
        let _ = writeln!(out, "  otlphttp/evald:");
        let _ = writeln!(out, "    endpoint: {}", t.http_endpoint());
    }
    if let Some(env) = &t.token_env {
        let _ = writeln!(out, "    headers:");
        let _ = writeln!(out, "      Authorization: \"Bearer ${{env:{env}}}\"");
    }
    let name = if t.grpc {
        "otlp/evald"
    } else {
        "otlphttp/evald"
    };
    let _ = writeln!(out, "service:");
    let _ = writeln!(out, "  pipelines:");
    let _ = writeln!(out, "    traces:");
    let _ = writeln!(out, "      exporters: [<your exporter>, {name}]");
    let _ = writeln!(out, "    logs:");
    let _ = writeln!(out, "      exporters: [<your exporter>, {name}]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target::from_addrs("127.0.0.1:4318", "127.0.0.1:4317").unwrap()
    }

    #[test]
    fn claude_code_defaults_to_events_over_grpc_with_logs_exported_first() {
        let s = snippet("claude-code", &target()).unwrap();
        assert!(s.contains("export CLAUDE_CODE_ENABLE_TELEMETRY=1"));
        assert!(s.contains("export OTEL_LOGS_EXPORTER=otlp"));
        assert!(s.contains("export OTEL_EXPORTER_OTLP_PROTOCOL=grpc"));
        assert!(s.contains("export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317"));
        assert!(s.contains("export OTEL_LOGS_EXPORT_INTERVAL=1000"));
        assert!(
            s.contains("# export OTEL_TRACES_EXPORTER=otlp"),
            "traces offered, off"
        );
        assert!(!s.contains("HEADERS"), "no auth asked for, none printed");
    }

    #[test]
    fn claude_code_over_http_with_traces_and_a_token_variable() {
        let mut t = target();
        t.grpc = false;
        t.traces = true;
        t.token_env = Some("EVALD_TOKEN".into());
        let s = snippet("claude-code", &t).unwrap();
        assert!(s.contains("export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf"));
        assert!(s.contains("export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318"));
        assert!(s.contains("export CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1"));
        assert!(s.contains("export OTEL_TRACES_EXPORTER=otlp"));
        assert!(
            s.contains("Authorization=Bearer ${EVALD_TOKEN}"),
            "the variable, never a token"
        );
    }

    #[test]
    fn claude_code_is_told_where_evalds_mcp_endpoint_is() {
        let s = snippet("claude-code", &target()).unwrap();
        assert!(
            s.contains("# claude mcp add --transport http evald http://127.0.0.1:4318/mcp\n"),
            "{s}"
        );
        let mut t = target();
        t.token_env = Some("EVALD_TOKEN".into());
        let s = snippet("claude-code", &t).unwrap();
        assert!(
            s.contains("/mcp --header \"Authorization: Bearer ${EVALD_TOKEN}\""),
            "{s}"
        );
    }

    #[test]
    fn claude_code_settings_json_is_valid_json_after_its_comment_lines() {
        let mut t = target();
        t.settings_json = true;
        let s = snippet("claude-code", &t).unwrap();
        let json: String = s.lines().filter(|l| !l.starts_with("//")).collect();
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["env"]["OTEL_LOGS_EXPORTER"], "otlp");
        assert_eq!(
            v["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"],
            "http://127.0.0.1:4317"
        );
    }

    #[test]
    fn a_wildcard_bind_becomes_a_dialable_address() {
        let t = Target::from_addrs("0.0.0.0:4318", "0.0.0.0:4317").unwrap();
        assert_eq!(t.host, "127.0.0.1");
        let t = Target::from_addrs("[::1]:9318", "[::1]:9317").unwrap();
        assert_eq!(t.http_endpoint(), "http://[::1]:9318");
    }

    #[test]
    fn the_collector_snippet_fans_out_both_signals() {
        let s = snippet("collector", &target()).unwrap();
        assert!(s.contains("otlp/evald:"));
        assert!(s.contains("endpoint: 127.0.0.1:4317"));
        assert_eq!(s.matches("[<your exporter>, otlp/evald]").count(), 2);
        let mut http = target();
        http.grpc = false;
        assert!(snippet("collector", &http)
            .unwrap()
            .contains("endpoint: http://127.0.0.1:4318"));
    }

    /// `serve --otlp-grpc ""` turns gRPC off; `connect` read from the same settings says HTTP.
    #[test]
    fn an_empty_grpc_address_means_http() {
        let t = Target::from_addrs("127.0.0.1:4318", "").unwrap();
        assert!(!t.grpc);
        let s = snippet("claude-code", &t).unwrap();
        assert!(
            s.contains("OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318"),
            "{s}"
        );
        assert!(Target::from_addrs("127.0.0.1:4318", "not an address").is_err());
    }

    #[test]
    fn an_unknown_tool_names_the_known_ones() {
        let e = snippet("cursor", &target()).unwrap_err().to_string();
        assert!(
            e.contains("claude-code") && e.contains("codex") && e.contains("gemini-cli"),
            "{e}"
        );
    }

    #[test]
    fn codex_sends_its_events_over_grpc_with_tool_output_kept_out() {
        let s = snippet("codex", &target()).unwrap();
        assert!(s.contains("[otel]"));
        assert!(s.contains(r#"exporter = { otlp-grpc = { endpoint = "http://127.0.0.1:4317" } }"#));
        assert!(s.contains("log_user_prompt = false"));
        assert!(s.contains("tool_result = { max_bytes = 0 }"));
        assert!(
            s.contains("exit code"),
            "the snippet says what keeping output out costs"
        );
        assert!(s.contains("# trace_exporter ="), "traces offered, off");
        assert!(
            !s.contains("\nmetrics_exporter"),
            "metrics left as the user has them"
        );
        assert!(!s.contains("authorization"));
        // Every line is TOML or a comment: the snippet pastes as is.
        for line in s.lines() {
            assert!(
                line.starts_with('#') || line == "[otel]" || line.contains(" = "),
                "{line}"
            );
        }
    }

    #[test]
    fn codex_over_http_names_each_signals_path_and_a_token_placeholder() {
        let mut t = target();
        t.grpc = false;
        t.traces = true;
        t.token_env = Some("EVALD_TOKEN".into());
        let s = snippet("codex", &t).unwrap();
        assert!(s.contains(
            r#"exporter = { otlp-http = { endpoint = "http://127.0.0.1:4318/v1/logs", protocol = "binary", headers = { authorization = "Bearer <your evald token>" } } }"#
        ));
        assert!(s.contains(
            r#"trace_exporter = { otlp-http = { endpoint = "http://127.0.0.1:4318/v1/traces""#
        ));
        assert!(s.contains("$EVALD_TOKEN"), "says which token");
    }

    #[test]
    fn gemini_cli_turns_its_prompt_logging_off() {
        let s = snippet("gemini-cli", &target()).unwrap();
        for line in [
            "export GEMINI_TELEMETRY_ENABLED=true",
            "export GEMINI_TELEMETRY_TARGET=local",
            "export GEMINI_TELEMETRY_OTLP_PROTOCOL=grpc",
            "export GEMINI_TELEMETRY_OTLP_ENDPOINT=http://127.0.0.1:4317",
            "export GEMINI_TELEMETRY_LOG_PROMPTS=false",
            "export OTEL_TRACES_SAMPLER=always_off",
        ] {
            assert!(s.lines().any(|l| l == line), "{line} in\n{s}");
        }
        assert_eq!(snippet("gemini", &target()).unwrap(), s, "an alias");
    }

    #[test]
    fn gemini_cli_settings_json_over_http_with_a_token() {
        let mut t = target();
        t.grpc = false;
        t.traces = true;
        t.token_env = Some("EVALD_TOKEN".into());
        let exports = snippet("gemini-cli", &t).unwrap();
        assert!(exports.contains("export GEMINI_TELEMETRY_OTLP_PROTOCOL=http"));
        assert!(exports.contains("export GEMINI_TELEMETRY_OTLP_ENDPOINT=http://127.0.0.1:4318"));
        assert!(exports.contains(
            "export GEMINI_TELEMETRY_OTLP_HEADERS=\"Authorization=Bearer ${EVALD_TOKEN}\""
        ));
        assert!(
            !exports.contains("OTEL_TRACES_SAMPLER"),
            "--traces keeps the spans"
        );
        assert!(
            !exports.contains("export GEMINI_TELEMETRY_TRACES_ENABLED"),
            "never: it adds content"
        );
        t.settings_json = true;
        let s = snippet("gemini-cli", &t).unwrap();
        let json: String = s.lines().filter(|l| !l.starts_with("//")).collect();
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let tel = &v["telemetry"];
        assert_eq!(tel["logPrompts"], false);
        assert_eq!(tel["otlpProtocol"], "http");
        assert!(tel.get("traces").is_none(), "a content switch, never set");
        assert_eq!(
            tel["otlpHeaders"]["Authorization"],
            "Bearer <your evald token>"
        );
    }
}
