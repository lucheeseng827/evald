//! Judge endpoints as configuration (ROADMAP-0.4-0.6 5.1).
//!
//! An endpoint is where a judge's model calls go: a hosted provider, or the team's own
//! OpenAI-compatible server (vLLM, Ollama, llama.cpp server, TGI). It is declared once, by name,
//! in the eval YAML's `endpoints:` block, and a judge names it:
//!
//! ```yaml
//! endpoints:
//!   - name: gpu-box
//!     api: openai-chat               # or anthropic-messages
//!     base_url: http://gpu-box:8000/v1  # with or without the trailing /v1
//!     # api_key_env: VLLM_KEY        # optional: a local server needs no key
//!     # ca_file: internal-ca.pem     # a private CA; client_cert + client_key for mTLS
//!     timeout_secs: 120
//!     max_in_flight: 1
//! judges:
//!   - rail: qa_correctness
//!     endpoint: gpu-box
//!     model: Qwen/Qwen2.5-7B-Instruct
//! ```
//!
//! A judge that still says `provider: anthropic | openai` gets the implicit endpoint of that
//! name, built from the environment exactly as before (`*_BASE_URL`, `*_API_KEY`), so existing
//! configs keep working. Everything here is pure configuration: it is in the default build and
//! makes no call. The HTTP client that uses it is behind the `judge` feature.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// The wire API an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Api {
    /// `POST /v1/chat/completions`: OpenAI, and every OpenAI-compatible server.
    OpenaiChat,
    /// `POST /v1/messages`: Anthropic.
    AnthropicMessages,
}

impl Api {
    pub fn as_str(self) -> &'static str {
        match self {
            Api::OpenaiChat => "openai-chat",
            Api::AnthropicMessages => "anthropic-messages",
        }
    }
}

/// How a judge asks the endpoint for a JSON score. The reply is parsed tolerantly whatever the
/// mode, so `none` still works with a model that follows the prompt's JSON instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutput {
    /// `json_schema`, stepping down to `json_object` and then to none when the server refuses
    /// the one before. What a server accepted is remembered for the rest of the run.
    #[default]
    Auto,
    /// OpenAI `response_format: json_schema` (vLLM guided decoding, Ollama, llama.cpp server).
    JsonSchema,
    /// OpenAI `response_format: json_object`.
    JsonObject,
    /// Ask for nothing: the prompt's own instruction, and the tolerant parse.
    None,
}

impl StructuredOutput {
    pub fn as_str(self) -> &'static str {
        match self {
            StructuredOutput::Auto => "auto",
            StructuredOutput::JsonSchema => "json_schema",
            StructuredOutput::JsonObject => "json_object",
            StructuredOutput::None => "none",
        }
    }
}

fn default_timeout_secs() -> u64 {
    120
}

fn default_retries() -> u32 {
    3
}

fn default_max_in_flight() -> usize {
    1
}

/// One `endpoints:` entry, as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSpec {
    pub name: String,
    pub api: Api,
    pub base_url: String,
    /// The environment variable holding the key. Absent: the endpoint takes no key (a local
    /// server). The key itself is read at call time only and is never logged or cached.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// A PEM file of CA certificates to trust, for an endpoint behind a private CA.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// mTLS: a PEM client certificate (chain) and its private key, both or neither.
    #[serde(default)]
    pub client_cert: Option<PathBuf>,
    #[serde(default)]
    pub client_key: Option<PathBuf>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Judge calls this endpoint may have in flight at once. 1 by default: a judge sharing a GPU
    /// with production must not crowd it.
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
    /// Extra request headers (a gateway's routing or team header). Not for keys: use
    /// `api_key_env`, which keeps the key out of the config file.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// How to ask for a JSON score (OpenAI-compatible endpoints; Anthropic is prompted).
    #[serde(default)]
    pub structured_output: StructuredOutput,
    /// Retries of one call after a 408, a 429, a 500/502/503/504, Anthropic's 529 or a dropped
    /// connection, with backoff. 0 = none.
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// Where this endpoint's data goes, as you label it (`MY`, `EU`, `on-prem`): recorded on every
    /// call in the egress ledger and in each run's manifest. evald does not check it.
    #[serde(default)]
    pub jurisdiction: Option<String>,
}

/// A resolved endpoint: validated, its base URL normalised, its paths absolute.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub name: String,
    pub api: Api,
    /// No trailing `/` and no trailing `/v1`: the client appends `/v1/...` itself, so
    /// `http://host:8000` and `http://host:8000/v1/` are the same endpoint (and the same cache).
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub ca_file: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    pub timeout: Duration,
    pub max_in_flight: usize,
    pub headers: BTreeMap<String, String>,
    pub structured_output: StructuredOutput,
    pub retries: u32,
    pub jurisdiction: Option<String>,
}

/// The names of the implicit endpoints a judge reaches with `provider:`.
pub const IMPLICIT_ANTHROPIC: &str = "anthropic";
pub const IMPLICIT_OPENAI: &str = "openai";

/// `http://host:8000/v1/` → `http://host:8000`. Rejects anything that is not an http(s) URL with
/// a host, because a typo here otherwise surfaces as a confusing connection error mid-run.
pub fn normalize_base_url(raw: &str) -> anyhow::Result<String> {
    let mut url = raw.trim().trim_end_matches('/').to_string();
    if let Some(stripped) = url.strip_suffix("/v1") {
        url = stripped.trim_end_matches('/').to_string();
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("base_url {raw:?} must start with http:// or https://"))?;
    let host = rest.split('/').next().unwrap_or("");
    // Not echoed: the URL holds the password. A base URL is printed and stored in the clear (the
    // run manifest, every judge span's server.address, `judge probe`, error messages).
    anyhow::ensure!(
        !host.contains('@'),
        "base_url carries credentials (`user:password@`), which would be stored and printed with \
         it; put the key in api_key_env"
    );
    anyhow::ensure!(
        !host.is_empty() && !host.contains(char::is_whitespace),
        "base_url {raw:?} has no host"
    );
    Ok(url)
}

impl EndpointSpec {
    /// Validate and resolve, with relative file paths taken against `base_dir` (the eval
    /// config's directory, as the dataset path is).
    pub fn resolve(&self, base_dir: &Path) -> anyhow::Result<Endpoint> {
        let name = self.name.trim();
        anyhow::ensure!(!name.is_empty(), "an endpoint needs a name");
        anyhow::ensure!(
            name != IMPLICIT_ANTHROPIC && name != IMPLICIT_OPENAI,
            "endpoint name {name:?} is reserved for the implicit `provider: {name}` endpoint"
        );
        let base_url = normalize_base_url(&self.base_url)
            .map_err(|e| anyhow::anyhow!("endpoint {name:?}: {e}"))?;
        anyhow::ensure!(
            self.client_cert.is_some() == self.client_key.is_some(),
            "endpoint {name:?}: client_cert and client_key go together (mTLS needs both)"
        );
        anyhow::ensure!(
            self.timeout_secs > 0,
            "endpoint {name:?}: timeout_secs must be at least 1"
        );
        anyhow::ensure!(
            self.max_in_flight > 0,
            "endpoint {name:?}: max_in_flight must be at least 1"
        );
        anyhow::ensure!(
            self.retries <= 10,
            "endpoint {name:?}: retries is at most 10 (got {})",
            self.retries
        );
        if let Some(var) = &self.api_key_env {
            anyhow::ensure!(
                !var.trim().is_empty(),
                "endpoint {name:?}: api_key_env names no variable"
            );
        }
        for key in self.headers.keys() {
            let lower = key.to_ascii_lowercase();
            anyhow::ensure!(
                !matches!(lower.as_str(), "authorization" | "x-api-key"),
                "endpoint {name:?}: put the key in api_key_env, not in a `{key}` header \
                 (a header is written in the config file)"
            );
        }
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| resolve_path(base_dir, p));
        Ok(Endpoint {
            name: name.to_string(),
            api: self.api,
            base_url,
            api_key_env: self.api_key_env.clone(),
            ca_file: path(&self.ca_file),
            client_cert: path(&self.client_cert),
            client_key: path(&self.client_key),
            timeout: Duration::from_secs(self.timeout_secs),
            max_in_flight: self.max_in_flight,
            headers: self.headers.clone(),
            structured_output: self.structured_output,
            retries: self.retries,
            jurisdiction: self.jurisdiction.clone(),
        })
    }
}

fn resolve_path(base_dir: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

impl Endpoint {
    /// The endpoint `provider: anthropic | openai` has always meant: the provider's API, its
    /// `*_BASE_URL` override if set (now with or without `/v1`), and its key variable.
    pub fn implicit(api: Api) -> anyhow::Result<Endpoint> {
        let (name, base_var, default_base, key_var) = match api {
            Api::AnthropicMessages => (
                IMPLICIT_ANTHROPIC,
                "ANTHROPIC_BASE_URL",
                "https://api.anthropic.com",
                "ANTHROPIC_API_KEY",
            ),
            Api::OpenaiChat => (
                IMPLICIT_OPENAI,
                "OPENAI_BASE_URL",
                "https://api.openai.com",
                "OPENAI_API_KEY",
            ),
        };
        let raw = std::env::var(base_var)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| default_base.to_string());
        let base_url = normalize_base_url(&raw).map_err(|e| anyhow::anyhow!("{base_var}: {e}"))?;
        Ok(Endpoint {
            name: name.to_string(),
            api,
            base_url,
            api_key_env: Some(key_var.to_string()),
            ca_file: None,
            client_cert: None,
            client_key: None,
            timeout: Duration::from_secs(default_timeout_secs()),
            max_in_flight: default_max_in_flight(),
            headers: BTreeMap::new(),
            structured_output: StructuredOutput::Auto,
            retries: default_retries(),
            jurisdiction: None,
        })
    }

    /// The URL of a path under the API root, e.g. `url("/v1/models")`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

/// Resolve a config's `endpoints:` block into a name → endpoint map, rejecting duplicates.
pub fn resolve_all(
    specs: &[EndpointSpec],
    base_dir: &Path,
) -> anyhow::Result<BTreeMap<String, Endpoint>> {
    let mut out = BTreeMap::new();
    for spec in specs {
        let ep = spec.resolve(base_dir)?;
        anyhow::ensure!(
            !out.contains_key(&ep.name),
            "endpoint {:?} is declared twice",
            ep.name
        );
        out.insert(ep.name.clone(), ep);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(base_url: &str) -> EndpointSpec {
        EndpointSpec {
            name: "gpu".into(),
            api: Api::OpenaiChat,
            base_url: base_url.into(),
            api_key_env: None,
            ca_file: None,
            client_cert: None,
            client_key: None,
            timeout_secs: 120,
            max_in_flight: 1,
            headers: BTreeMap::new(),
            structured_output: StructuredOutput::Auto,
            retries: 3,
            jurisdiction: None,
        }
    }

    #[test]
    fn a_base_url_is_the_same_with_or_without_v1() {
        for raw in [
            "http://gpu:8000",
            "http://gpu:8000/",
            "http://gpu:8000/v1",
            "http://gpu:8000/v1/",
            " http://gpu:8000/v1// ",
        ] {
            assert_eq!(
                normalize_base_url(raw).unwrap(),
                "http://gpu:8000",
                "{raw:?}"
            );
        }
        // A path before /v1 is kept: a gateway mounting the API under a prefix.
        assert_eq!(
            normalize_base_url("https://gw.internal/llm/v1").unwrap(),
            "https://gw.internal/llm"
        );
        // Only a trailing /v1 is stripped.
        assert_eq!(
            normalize_base_url("http://gpu:8000/v1beta").unwrap(),
            "http://gpu:8000/v1beta"
        );
    }

    #[test]
    fn a_base_url_must_be_http_with_a_host() {
        for bad in ["gpu:8000", "ftp://gpu", "http://", "https:///v1", ""] {
            assert!(
                normalize_base_url(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
        let err = normalize_base_url("https://svc:s3cr3t@gw.internal/v1")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("credentials") && !err.contains("s3cr3t"),
            "{err}"
        );
    }

    #[test]
    fn resolve_checks_mtls_pairs_limits_and_key_headers() {
        let dir = Path::new("/etc/evald");
        let ok = spec("http://gpu:8000/v1").resolve(dir).unwrap();
        assert_eq!(ok.base_url, "http://gpu:8000");
        assert_eq!(ok.url("/v1/models"), "http://gpu:8000/v1/models");
        assert_eq!(ok.api_key_env, None, "keyless is allowed");

        let mut half = spec("http://gpu");
        half.client_cert = Some("client.pem".into());
        assert!(half.resolve(dir).is_err(), "a cert without its key");
        half.client_key = Some("client.key".into());
        let ep = half.resolve(dir).unwrap();
        assert_eq!(
            ep.client_cert.as_deref(),
            Some(Path::new("/etc/evald/client.pem"))
        );

        let mut zero = spec("http://gpu");
        zero.max_in_flight = 0;
        assert!(zero.resolve(dir).is_err());

        let mut header_key = spec("http://gpu");
        header_key
            .headers
            .insert("Authorization".into(), "Bearer x".into());
        assert!(
            header_key.resolve(dir).is_err(),
            "keys belong in api_key_env"
        );

        let mut reserved = spec("http://gpu");
        reserved.name = "openai".into();
        assert!(reserved.resolve(dir).is_err());
    }

    #[test]
    fn an_unknown_field_is_an_error_not_ignored() {
        let yaml = "name: gpu\napi: openai-chat\nbase_url: http://gpu\napi_key: sk-oops\n";
        assert!(serde_yaml::from_str::<EndpointSpec>(yaml).is_err());
    }

    #[test]
    fn duplicate_names_are_refused() {
        let specs = vec![spec("http://a"), spec("http://b")];
        assert!(resolve_all(&specs, Path::new(".")).is_err());
    }
}
