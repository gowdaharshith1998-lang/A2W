//! [`NodeKind::HttpRequest`] executor.
//!
//! Side-effecting. Params:
//! ```json
//! { "method": "GET|POST|...", "url": "...", "headers": {..}?, "json": <body>? }
//! ```
//! Produces one output item per input item, shaped
//! `{ "status": <u16>, "body": <parsed json or string> }`.
//!
//! A minimal interim templating helper substitutes `{{json.FIELD}}` / `{{json}}`
//! tokens (from the corresponding input item) into the `url` and any string in
//! the `json` body. The full expression engine arrives later; this helper is
//! intentionally small (see `template.rs`).
//!
//! `dry_run` returns a mocked item per input item WITHOUT any network call.

use async_trait::async_trait;
use reqwest::Client;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

use crate::template;

/// Executor for [`a2w_ir::NodeKind::HttpRequest`]. Holds a shared client so
/// connection pooling is reused across all HTTP nodes/items.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    client: Client,
}

impl Default for HttpRequest {
    fn default() -> Self {
        Self {
            client: Client::new(),
        }
    }
}

impl HttpRequest {
    /// Construct with an explicit shared client.
    #[must_use]
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    /// Resolve the request URL for one input item (templated).
    fn resolve_url(params: &serde_json::Value, item: &serde_json::Value) -> Result<String, NodeError> {
        let raw = params
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| NodeError::BadParams("HttpRequest requires a string `url`".into()))?;
        Ok(template::render(raw, item))
    }

    /// Parse the HTTP method from params (defaults to GET).
    fn resolve_method(params: &serde_json::Value) -> Result<reqwest::Method, NodeError> {
        let raw = params
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET");
        raw.parse::<reqwest::Method>()
            .map_err(|_| NodeError::BadParams(format!("invalid HTTP method '{raw}'")))
    }
}

#[async_trait]
impl NodeExecutor for HttpRequest {
    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: &NodeContext,
        input: Vec<Item>,
    ) -> Result<Vec<Item>, NodeError> {
        let method = Self::resolve_method(&ctx.params)?;

        let mut out = Vec::with_capacity(input.len());
        for item in &input {
            let url = Self::resolve_url(&ctx.params, &item.json)?;
            let mut req = self.client.request(method.clone(), &url);

            // Optional headers (string -> string).
            if let Some(serde_json::Value::Object(headers)) = ctx.params.get("headers") {
                for (k, v) in headers {
                    if let Some(val) = v.as_str() {
                        req = req.header(k.as_str(), val);
                    } else {
                        return Err(NodeError::BadParams(format!(
                            "header '{k}' must be a string"
                        )));
                    }
                }
            }

            // Optional JSON body, with string templating applied recursively.
            if let Some(body) = ctx.params.get("json") {
                let rendered = render_json(body, &item.json);
                req = req.json(&rendered);
            }

            let resp = req
                .send()
                .await
                .map_err(|e| NodeError::Http(e.to_string()))?;
            let status = resp.status().as_u16();
            // Read the body once, then try to parse it as JSON; fall back to a
            // string so non-JSON responses still flow.
            let text = resp
                .text()
                .await
                .map_err(|e| NodeError::Http(e.to_string()))?;
            let body = serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or(serde_json::Value::String(text));

            out.push(Item::produced(
                serde_json::json!({ "status": status, "body": body }),
                ctx.node_id.clone(),
                0,
            ));
        }
        Ok(out)
    }

    async fn dry_run(
        &self,
        ctx: &NodeContext,
        input: Vec<Item>,
    ) -> Result<Vec<Item>, NodeError> {
        // No network: produce a faithful mock per input item, echoing the
        // resolved URL so the run shape can be inspected.
        let mut out = Vec::with_capacity(input.len());
        for item in &input {
            let url = Self::resolve_url(&ctx.params, &item.json)?;
            out.push(Item::produced(
                serde_json::json!({ "_mock": true, "status": 200, "url": url }),
                ctx.node_id.clone(),
                0,
            ));
        }
        Ok(out)
    }
}

/// Recursively apply string templating to a JSON body against `item`.
///
/// Strings are rendered through the token substituter; containers recurse;
/// scalars pass through.
fn render_json(value: &serde_json::Value, item: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(template::render(s, item)),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| render_json(v, item)).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                out.insert(k.clone(), render_json(v, item));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}
