// ureq is a blocking HTTP client that depends on std::net and OS threads.
// It cannot work on wasm32 targets — users must provide their own HttpClient.
#![cfg(not(target_arch = "wasm32"))]

use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;
use wacore::net::{HttpClient, HttpRequest, HttpResponse, StreamingHttpResponse};

/// HTTP client implementation using `ureq` for synchronous HTTP requests.
/// Since `ureq` is blocking, all requests are wrapped in `tokio::task::spawn_blocking`.
#[derive(Debug, Clone)]
pub struct UreqHttpClient {
    agent: ureq::Agent,
}

impl UreqHttpClient {
    pub fn new() -> Self {
        let agent = build_agent();
        Self { agent }
    }
}

impl Default for UreqHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

fn build_agent() -> ureq::Agent {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
    const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
    const BODY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
    const OVERALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

    #[cfg(feature = "danger-skip-tls-verify")]
    {
        use ureq::config::Config;
        use ureq::tls::TlsConfig;
        Config::builder()
            .tls_config(TlsConfig::builder().disable_verification(true).build())
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .timeout_recv_body(Some(BODY_TIMEOUT))
            .timeout_global(Some(OVERALL_TIMEOUT))
            .build()
            .into()
    }

    #[cfg(not(feature = "danger-skip-tls-verify"))]
    {
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .timeout_recv_body(Some(BODY_TIMEOUT))
            .timeout_global(Some(OVERALL_TIMEOUT))
            .build()
            .into()
    }
}

#[async_trait]
impl HttpClient for UreqHttpClient {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        let agent = self.agent.clone();
        // Since ureq is blocking, we must use spawn_blocking
        tokio::task::spawn_blocking(move || {
            let response = match request.method.as_str() {
                "GET" => {
                    let mut req = agent.get(&request.url);
                    for (key, value) in &request.headers {
                        req = req.header(key, value);
                    }
                    req.call()?
                }
                "POST" => {
                    let mut req = agent.post(&request.url);
                    for (key, value) in &request.headers {
                        req = req.header(key, value);
                    }
                    if let Some(body) = request.body {
                        req.send(&body[..])?
                    } else {
                        req.send(&[])?
                    }
                }
                method => {
                    return Err(anyhow::anyhow!("Unsupported HTTP method: {}", method));
                }
            };

            let status_code = response.status().as_u16();

            // Read the response body. ureq 3.x's read_to_vec() defaults to a
            // 10 MB cap ("response body is larger than request limit: 10485760"),
            // which silently failed large WhatsApp media — e.g. architectural-
            // drawing PDFs over 10 MB never downloaded and showed a dead-end
            // placeholder. WhatsApp documents go well beyond that; lift the cap
            // Generic responses are protocol metadata and should stay small.
            // Large WhatsApp media uses `execute_streaming`, below, so keeping
            // this limit tight prevents a bad endpoint from causing an RSS spike.
            let mut body = response.into_body();
            let body_bytes = body.with_config().limit(32 * 1024 * 1024).read_to_vec()?;

            Ok(HttpResponse {
                status_code,
                body: body_bytes,
            })
        })
        .await?
    }

    fn execute_streaming(&self, request: HttpRequest) -> Result<StreamingHttpResponse> {
        // Note: no spawn_blocking here — this is called FROM within spawn_blocking
        // by the streaming download code. The entire HTTP fetch + decrypt happens
        // in one blocking thread.
        let response = match request.method.as_str() {
            "GET" => {
                let mut req = self.agent.get(&request.url);
                for (key, value) in &request.headers {
                    req = req.header(key, value);
                }
                req.call()?
            }
            method => {
                return Err(anyhow::anyhow!(
                    "Streaming only supports GET, got: {}",
                    method
                ));
            }
        };

        let status_code = response.status().as_u16();
        let reader = response.into_body().into_reader();

        Ok(StreamingHttpResponse {
            status_code,
            body: Box::new(reader),
        })
    }
}
