//! # a2w-openapi
//!
//! Milestone **M5** — turn an **OpenAPI 3.x specification (JSON)** into an A2W
//! [`Integration`] manifest: a curated, agent-facing list of callable
//! [`Action`]s, each with an HTTP binding (method + path) and a JSON-Schema
//! describing its inputs. This is the "any API becomes the catalog" generator.
//!
//! The spec is treated as **untrusted**: the parser never panics. Structural
//! problems that prevent any manifest from being produced (invalid JSON, no
//! `paths`) surface as a clean [`OpenApiError`]; everything lossy or skippable
//! (a missing server URL, an operation without an `operationId`, an unresolvable
//! `$ref`, a malformed path/operation object) is recorded as a warning on a
//! successful [`GenerateResult`] and the rest of the spec is still processed.
//!
//! ## Entry point
//! [`generate`] takes the raw spec JSON and returns a [`GenerateResult`] holding
//! the [`Integration`] plus the warnings.
//!
//! ## How the input schema for each action is assembled
//! The `input_schema` of an [`Action`] is a JSON-Schema object of the shape
//! `{ "type": "object", "properties": { ... }, "required": [ ... ] }` built by
//! merging, in order:
//!
//! 1. **Path-level parameters** (`paths.<path>.parameters`) then
//!    **operation-level parameters** (`paths.<path>.<method>.parameters`).
//!    Operation-level entries override path-level ones with the same
//!    `(name, in)` identity, matching the OpenAPI precedence rule. Each
//!    parameter contributes one property keyed by its `name`; the property value
//!    is the parameter's `schema` if present, otherwise `{"type":"string"}`. The
//!    property name is annotated with the parameter location via a non-standard
//!    `"x-in"` key (query / path / header / cookie) so the binding layer knows
//!    where each value goes. A parameter marked `"required": true` (always true
//!    for `in: path`) is added to the schema's `required` list.
//! 2. **Request body**: `requestBody.content["application/json"].schema`, if
//!    present, is folded in under a single `body` property. A top-level `$ref`
//!    on that schema is resolved **one level deep** against
//!    `components.schemas.<Name>`; if it cannot be resolved the original
//!    `{"$ref": ...}` is kept verbatim and a warning is recorded. If the request
//!    body is marked `required`, `body` is added to the schema's `required` list.
//!
//! ## Access classification
//! Each action is tagged with an [`Access`] level derived purely from the HTTP
//! method: `GET`/`HEAD`/`OPTIONS` -> [`Access::Read`], `POST`/`PUT`/`PATCH` ->
//! [`Access::Write`], `DELETE` -> [`Access::Destructive`].

#![forbid(unsafe_code)]

mod slug;

use serde::Serialize;
use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::slug::{slugify, synth_name};

/// The HTTP methods we surface as callable actions, in a stable order so the
/// generated action list is deterministic for a given spec.
const HTTP_METHODS: [&str; 7] = ["get", "put", "post", "delete", "patch", "head", "options"];

/// Error returned when an OpenAPI spec cannot be turned into a manifest at all.
///
/// Note that *operation-level* and *cosmetic* problems are not errors — they
/// surface as warnings on a successful [`GenerateResult`]. An `OpenApiError` is
/// reserved for inputs that are not valid JSON or lack the `paths` object the
/// generator fundamentally requires.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenApiError {
    /// The input was not valid JSON.
    #[error("invalid OpenAPI JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The input was valid JSON but not a usable OpenAPI 3.x document.
    #[error("malformed OpenAPI spec: {0}")]
    Malformed(String),
}

/// The access level an [`Action`] requires, inferred from its HTTP method.
/// Serialized `snake_case` (`read` / `write` / `destructive`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// A safe, side-effect-free read (GET / HEAD / OPTIONS).
    Read,
    /// A mutating call that creates or updates state (POST / PUT / PATCH).
    Write,
    /// A destructive call that removes state (DELETE).
    Destructive,
}

impl Access {
    /// Classify an HTTP method (case-insensitive) into an access level.
    ///
    /// `GET`/`HEAD`/`OPTIONS` are reads, `POST`/`PUT`/`PATCH` are writes,
    /// `DELETE` is destructive. Any other method defaults to [`Access::Write`]
    /// (the conservative choice: assume it may mutate).
    #[must_use]
    pub fn for_method(method: &str) -> Self {
        match method.to_ascii_uppercase().as_str() {
            "GET" | "HEAD" | "OPTIONS" => Access::Read,
            "DELETE" => Access::Destructive,
            // POST / PUT / PATCH and anything unrecognized: treat as a write.
            _ => Access::Write,
        }
    }
}

/// A single callable operation exposed by the integration, with its HTTP
/// binding and a JSON-Schema describing the inputs the agent must supply.
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    /// Stable, agent-facing name (the spec `operationId`, or a synthesized
    /// `<method>_<path-slug>` when none is present).
    pub name: String,
    /// Uppercased HTTP method, e.g. `GET`, `POST`.
    pub method: String,
    /// The path template as written in the spec, e.g. `/users/{id}`.
    pub path: String,
    /// Human-readable description (the operation `summary` or `description`).
    pub description: String,
    /// JSON-Schema object describing this action's inputs (parameters + body).
    pub input_schema: Value,
    /// The access level this action requires, inferred from `method`.
    pub access: Access,
}

/// An A2W integration manifest distilled from an OpenAPI document.
#[derive(Debug, Clone, Serialize)]
pub struct Integration {
    /// Slug of the spec title; stable id for the integration.
    pub id: String,
    /// Human-readable title (the spec `info.title`, default `"api"`).
    pub title: String,
    /// Base URL for requests (`servers[0].url`, or empty with a warning).
    pub base_url: String,
    /// The callable actions, in spec order (path order, then method order).
    pub actions: Vec<Action>,
}

/// The result of a successful generation: the integration manifest plus every
/// best-effort/lossy decision recorded as a warning. `warnings` is empty for a
/// perfectly clean spec.
#[derive(Debug, Clone, Serialize)]
pub struct GenerateResult {
    /// The generated integration manifest.
    pub integration: Integration,
    /// Non-fatal issues encountered while generating. Empty when none.
    pub warnings: Vec<String>,
}

/// Generate an A2W [`Integration`] manifest from an OpenAPI 3.x spec (JSON).
///
/// # Errors
/// Returns [`OpenApiError::Json`] if `spec_json` is not valid JSON, or
/// [`OpenApiError::Malformed`] if it is valid JSON but is not a usable OpenAPI
/// document (top-level value not an object, or missing/invalid `paths`).
pub fn generate(spec_json: &str) -> Result<GenerateResult, OpenApiError> {
    let root: Value = serde_json::from_str(spec_json)?;

    let root_obj = root.as_object().ok_or_else(|| {
        OpenApiError::Malformed("top-level value is not a JSON object".to_string())
    })?;

    // `paths` is mandatory: without it there is nothing to expose.
    let paths = root_obj
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| OpenApiError::Malformed("missing or non-object 'paths'".to_string()))?;

    let mut warnings: Vec<String> = Vec::new();

    // --- Identity -----------------------------------------------------------
    let title = root_obj
        .get("info")
        .and_then(Value::as_object)
        .and_then(|info| info.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("api")
        .to_string();
    let id = slugify(&title);

    // --- Base URL: servers[0].url, else "" + warning ------------------------
    let base_url = root_obj
        .get("servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(Value::as_object)
        .and_then(|srv| srv.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let base_url = match base_url {
        Some(url) => url,
        None => {
            warnings.push(
                "no 'servers[0].url' found; base_url is empty (requests will need a base \
                 URL supplied elsewhere)"
                    .to_string(),
            );
            String::new()
        }
    };

    // `components.schemas` is used to resolve top-level request-body `$ref`s.
    let components_schemas = root_obj
        .get("components")
        .and_then(Value::as_object)
        .and_then(|c| c.get("schemas"))
        .and_then(Value::as_object);

    // --- Actions ------------------------------------------------------------
    let mut actions: Vec<Action> = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (path, path_item) in paths {
        let Some(path_obj) = path_item.as_object() else {
            warnings.push(format!("path '{path}' is not an object; skipped"));
            continue;
        };

        // Path-level parameters apply to every operation under this path.
        let path_params = path_obj.get("parameters").and_then(Value::as_array);

        for method in HTTP_METHODS {
            let Some(op_value) = path_obj.get(method) else {
                continue;
            };
            let Some(op) = op_value.as_object() else {
                warnings.push(format!(
                    "operation '{} {}' is not an object; skipped",
                    method.to_ascii_uppercase(),
                    path
                ));
                continue;
            };

            let action = build_action(
                path,
                method,
                op,
                path_params,
                components_schemas,
                &mut used_names,
                &mut warnings,
            );
            actions.push(action);
        }
    }

    Ok(GenerateResult {
        integration: Integration {
            id,
            title,
            base_url,
            actions,
        },
        warnings,
    })
}

/// Build a single [`Action`] from one operation object.
fn build_action(
    path: &str,
    method: &str,
    op: &Map<String, Value>,
    path_params: Option<&Vec<Value>>,
    components_schemas: Option<&Map<String, Value>>,
    used_names: &mut std::collections::HashSet<String>,
    warnings: &mut Vec<String>,
) -> Action {
    let method_upper = method.to_ascii_uppercase();

    // --- Name: operationId, else synthesized + warning ----------------------
    let raw_name = op.get("operationId").and_then(Value::as_str);
    let mut name = match raw_name {
        Some(id) if !id.trim().is_empty() => id.to_string(),
        _ => {
            warnings.push(format!(
                "operation '{method_upper} {path}' has no operationId; synthesized a name"
            ));
            synth_name(method, path)
        }
    };
    // Guarantee uniqueness so two operations never collide on the same name.
    if !used_names.insert(name.clone()) {
        let mut n: u32 = 2;
        let base = name.clone();
        loop {
            let candidate = format!("{base}_{n}");
            if used_names.insert(candidate.clone()) {
                warnings.push(format!(
                    "action name '{base}' was already used; renamed '{method_upper} {path}' \
                     to '{candidate}'"
                ));
                name = candidate;
                break;
            }
            n += 1;
        }
    }

    // --- Description: summary, else description, else "" ---------------------
    let description = op
        .get("summary")
        .and_then(Value::as_str)
        .or_else(|| op.get("description").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();

    // --- Input schema -------------------------------------------------------
    let input_schema = build_input_schema(
        path,
        &method_upper,
        op,
        path_params,
        components_schemas,
        warnings,
    );

    Action {
        name,
        method: method_upper.clone(),
        path: path.to_string(),
        description,
        input_schema,
        access: Access::for_method(&method_upper),
    }
}

/// Assemble the JSON-Schema `{ "type":"object", "properties":{}, "required":[] }`
/// for an operation from its merged parameters and JSON request body.
fn build_input_schema(
    path: &str,
    method_upper: &str,
    op: &Map<String, Value>,
    path_params: Option<&Vec<Value>>,
    components_schemas: Option<&Map<String, Value>>,
    warnings: &mut Vec<String>,
) -> Value {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();

    // Merge path-level then operation-level parameters. Operation-level entries
    // with the same (name, in) identity override path-level ones. We track seen
    // identities to honor that precedence and to de-duplicate.
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    let op_params = op.get("parameters").and_then(Value::as_array);
    // Process operation-level FIRST so it wins on collisions; then fall back to
    // path-level for any (name, in) not already supplied.
    for params in [op_params, path_params].into_iter().flatten() {
        for param in params {
            apply_parameter(
                param,
                method_upper,
                path,
                &mut properties,
                &mut required,
                &mut seen,
                warnings,
            );
        }
    }

    // --- Request body folded under `body` -----------------------------------
    if let Some(request_body) = op.get("requestBody").and_then(Value::as_object) {
        let body_required = request_body
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let json_schema = request_body
            .get("content")
            .and_then(Value::as_object)
            .and_then(|content| content.get("application/json"))
            .and_then(Value::as_object)
            .and_then(|media| media.get("schema"));

        if let Some(schema) = json_schema {
            let resolved = resolve_ref(schema, components_schemas, method_upper, path, warnings);
            properties.insert("body".to_string(), resolved);
            if body_required {
                push_unique_required(&mut required, "body");
            }
        }
    }

    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": Value::Array(required),
    })
}

/// Fold one `parameters[]` entry into the schema's properties/required.
///
/// Skips non-object entries and entries without a usable `name`, recording a
/// warning in each case. Honors the `(name, in)` precedence captured in `seen`.
fn apply_parameter(
    param: &Value,
    method_upper: &str,
    path: &str,
    properties: &mut Map<String, Value>,
    required: &mut Vec<Value>,
    seen: &mut std::collections::HashSet<(String, String)>,
    warnings: &mut Vec<String>,
) {
    let Some(param_obj) = param.as_object() else {
        warnings.push(format!(
            "a parameter on '{method_upper} {path}' is not an object; skipped"
        ));
        return;
    };

    let Some(pname) = param_obj.get("name").and_then(Value::as_str) else {
        warnings.push(format!(
            "a parameter on '{method_upper} {path}' has no 'name'; skipped"
        ));
        return;
    };

    // `in` defaults to "query" if absent (loose handling of messy specs).
    let location = param_obj
        .get("in")
        .and_then(Value::as_str)
        .unwrap_or("query")
        .to_string();

    // Honor OpenAPI precedence: a given (name, in) is taken from the first
    // source we see (operation-level is processed first, so it wins).
    if !seen.insert((pname.to_string(), location.clone())) {
        return;
    }

    // Property schema: the parameter's own `schema`, else a permissive string.
    let mut prop_schema = param_obj
        .get("schema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "string" }));

    // Annotate location for the binding layer via a non-standard `x-in` key.
    // Best-effort: only when the schema is an object (it virtually always is).
    if let Value::Object(map) = &mut prop_schema {
        map.insert("x-in".to_string(), Value::String(location.clone()));
    }

    properties.insert(pname.to_string(), prop_schema);

    // `in: path` parameters are always required per the OpenAPI spec; otherwise
    // honor the explicit `required` flag.
    let is_required = location == "path"
        || param_obj
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if is_required {
        push_unique_required(required, pname);
    }
}

/// Resolve a top-level `$ref` ONE level deep against `components.schemas`.
///
/// If `schema` is `{"$ref": "#/components/schemas/<Name>"}` and `<Name>` exists,
/// returns a clone of that schema. Otherwise (no `$ref`, a non-local/unknown
/// `$ref`, or missing components) returns `schema` unchanged; in the
/// unresolvable-`$ref` case a warning is recorded so the caller knows the
/// `{"$ref": ...}` was left in place.
fn resolve_ref(
    schema: &Value,
    components_schemas: Option<&Map<String, Value>>,
    method_upper: &str,
    path: &str,
    warnings: &mut Vec<String>,
) -> Value {
    let Some(ref_str) = schema.get("$ref").and_then(Value::as_str) else {
        // Not a $ref: use as-is.
        return schema.clone();
    };

    const PREFIX: &str = "#/components/schemas/";
    if let Some(name) = ref_str.strip_prefix(PREFIX) {
        if let Some(resolved) = components_schemas.and_then(|s| s.get(name)) {
            return resolved.clone();
        }
    }

    warnings.push(format!(
        "request body schema $ref '{ref_str}' on '{method_upper} {path}' could not be \
         resolved against components.schemas; left as-is"
    ));
    schema.clone()
}

/// Push `name` into the `required` array unless already present.
fn push_unique_required(required: &mut Vec<Value>, name: &str) {
    let needle = Value::String(name.to_string());
    if !required.contains(&needle) {
        required.push(needle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locate an action by name in the integration.
    fn action<'a>(res: &'a GenerateResult, name: &str) -> &'a Action {
        res.integration
            .actions
            .iter()
            .find(|a| a.name == name)
            .unwrap_or_else(|| panic!("expected action named '{name}'"))
    }

    /// The required-array of an input schema as a set of strings.
    fn required_set(schema: &Value) -> std::collections::HashSet<String> {
        schema["required"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The canonical small spec used by the main test: title, one server, a
    /// GET with a query param, a POST with a JSON body, and a DELETE with a
    /// path param.
    const SPEC: &str = r#"
    {
      "openapi": "3.0.0",
      "info": { "title": "Pet Store" },
      "servers": [ { "url": "https://api.petstore.io/v1" } ],
      "paths": {
        "/users": {
          "get": {
            "operationId": "listUsers",
            "summary": "List users",
            "parameters": [
              { "name": "limit", "in": "query", "required": false,
                "schema": { "type": "integer" } }
            ]
          },
          "post": {
            "operationId": "createUser",
            "summary": "Create a user",
            "requestBody": {
              "required": true,
              "content": {
                "application/json": {
                  "schema": {
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": [ "name" ]
                  }
                }
              }
            }
          }
        },
        "/users/{id}": {
          "delete": {
            "operationId": "deleteUser",
            "summary": "Delete a user",
            "parameters": [
              { "name": "id", "in": "path", "required": true,
                "schema": { "type": "string" } }
            ]
          }
        }
      }
    }
    "#;

    #[test]
    fn parses_canonical_spec() {
        let res = generate(SPEC).expect("canonical spec generates");

        // Identity + base URL.
        assert_eq!(res.integration.title, "Pet Store");
        assert_eq!(res.integration.id, "pet_store");
        assert_eq!(res.integration.base_url, "https://api.petstore.io/v1");

        // Exactly three actions.
        assert_eq!(res.integration.actions.len(), 3);

        // listUsers: GET -> Read; has `limit` in properties (not required).
        let list = action(&res, "listUsers");
        assert_eq!(list.method, "GET");
        assert_eq!(list.access, Access::Read);
        assert_eq!(list.path, "/users");
        assert!(list.input_schema["properties"]["limit"].is_object());
        assert_eq!(
            list.input_schema["properties"]["limit"]["type"],
            json!("integer")
        );
        assert_eq!(
            list.input_schema["properties"]["limit"]["x-in"],
            json!("query")
        );
        assert!(!required_set(&list.input_schema).contains("limit"));

        // createUser: POST -> Write; body folded under `body`, body required.
        let create = action(&res, "createUser");
        assert_eq!(create.method, "POST");
        assert_eq!(create.access, Access::Write);
        let body = &create.input_schema["properties"]["body"];
        assert_eq!(body["type"], json!("object"));
        assert_eq!(body["properties"]["name"]["type"], json!("string"));
        assert!(required_set(&create.input_schema).contains("body"));

        // deleteUser: DELETE -> Destructive; `id` in properties AND required.
        let del = action(&res, "deleteUser");
        assert_eq!(del.method, "DELETE");
        assert_eq!(del.access, Access::Destructive);
        assert!(del.input_schema["properties"]["id"].is_object());
        assert_eq!(del.input_schema["properties"]["id"]["x-in"], json!("path"));
        assert!(required_set(&del.input_schema).contains("id"));

        // Clean spec: no warnings.
        assert!(
            res.warnings.is_empty(),
            "expected no warnings, got: {:?}",
            res.warnings
        );
    }

    #[test]
    fn missing_operation_id_synthesizes_name_and_warns() {
        let spec = r#"
        {
          "info": { "title": "NoOpId" },
          "servers": [ { "url": "https://x" } ],
          "paths": {
            "/widgets/{id}": {
              "get": { "summary": "Get a widget" }
            }
          }
        }
        "#;
        let res = generate(spec).expect("generates");
        assert_eq!(res.integration.actions.len(), 1);
        // Synthesized "<method>_<path-slug>".
        let a = &res.integration.actions[0];
        assert_eq!(a.name, "get_widgets_id");
        assert_eq!(a.access, Access::Read);
        assert!(
            res.warnings.iter().any(|w| w.contains("operationId")),
            "expected an operationId warning, got: {:?}",
            res.warnings
        );
    }

    #[test]
    fn ref_request_body_resolved_one_level() {
        let spec = r##"
        {
          "info": { "title": "Refs" },
          "servers": [ { "url": "https://x" } ],
          "paths": {
            "/things": {
              "post": {
                "operationId": "createThing",
                "requestBody": {
                  "content": {
                    "application/json": {
                      "schema": { "$ref": "#/components/schemas/Thing" }
                    }
                  }
                }
              }
            }
          },
          "components": {
            "schemas": {
              "Thing": {
                "type": "object",
                "properties": { "color": { "type": "string" } }
              }
            }
          }
        }
        "##;
        let res = generate(spec).expect("generates");
        let create = action(&res, "createThing");
        // The $ref was resolved into the concrete schema under `body`.
        let body = &create.input_schema["properties"]["body"];
        assert_eq!(body["type"], json!("object"));
        assert_eq!(body["properties"]["color"]["type"], json!("string"));
        assert!(body.get("$ref").is_none(), "ref should be resolved away");
        assert!(res.warnings.is_empty(), "no warning for a resolvable ref");
    }

    #[test]
    fn unresolvable_ref_kept_with_warning() {
        let spec = r##"
        {
          "info": { "title": "BadRef" },
          "servers": [ { "url": "https://x" } ],
          "paths": {
            "/things": {
              "post": {
                "operationId": "createThing",
                "requestBody": {
                  "content": {
                    "application/json": {
                      "schema": { "$ref": "#/components/schemas/Missing" }
                    }
                  }
                }
              }
            }
          }
        }
        "##;
        let res = generate(spec).expect("generates");
        let create = action(&res, "createThing");
        let body = &create.input_schema["properties"]["body"];
        // The original $ref is preserved verbatim.
        assert_eq!(body["$ref"], json!("#/components/schemas/Missing"));
        assert!(
            res.warnings.iter().any(|w| w.contains("$ref")),
            "expected a $ref warning, got: {:?}",
            res.warnings
        );
    }

    #[test]
    fn path_and_operation_parameters_merge_with_op_precedence() {
        let spec = r#"
        {
          "info": { "title": "Merge" },
          "servers": [ { "url": "https://x" } ],
          "paths": {
            "/items/{id}": {
              "parameters": [
                { "name": "id", "in": "path", "required": true,
                  "schema": { "type": "string" } },
                { "name": "trace", "in": "query",
                  "schema": { "type": "boolean" } }
              ],
              "get": {
                "operationId": "getItem",
                "parameters": [
                  { "name": "trace", "in": "query",
                    "schema": { "type": "string", "description": "operation override" } }
                ]
              }
            }
          }
        }
        "#;
        let res = generate(spec).expect("generates");
        let get = action(&res, "getItem");
        let props = &get.input_schema["properties"];
        // Path-level `id` is present and required.
        assert_eq!(props["id"]["type"], json!("string"));
        assert!(required_set(&get.input_schema).contains("id"));
        // `trace` is overridden by the operation-level entry (string, not bool).
        assert_eq!(props["trace"]["type"], json!("string"));
        assert_eq!(props["trace"]["description"], json!("operation override"));
    }

    #[test]
    fn missing_paths_is_malformed() {
        let spec = r#"{ "info": { "title": "x" }, "servers": [] }"#;
        assert!(matches!(generate(spec), Err(OpenApiError::Malformed(_))));
    }

    #[test]
    fn invalid_json_errors() {
        assert!(matches!(generate("{ not json"), Err(OpenApiError::Json(_))));
    }

    #[test]
    fn empty_servers_warns_and_base_url_blank() {
        let spec = r#"
        {
          "info": { "title": "NoServer" },
          "paths": {
            "/ping": { "get": { "operationId": "ping" } }
          }
        }
        "#;
        let res = generate(spec).expect("generates");
        assert_eq!(res.integration.base_url, "");
        assert!(
            res.warnings.iter().any(|w| w.contains("servers")),
            "expected a servers warning, got: {:?}",
            res.warnings
        );
    }

    #[test]
    fn defaults_title_and_skips_non_object_operation() {
        // No info.title -> "api"; a non-object operation is skipped with a warning.
        let spec = r#"
        {
          "paths": {
            "/a": {
              "get": "not an object",
              "post": { "operationId": "doPost" }
            },
            "/b": "not an object"
          }
        }
        "#;
        let res = generate(spec).expect("generates");
        assert_eq!(res.integration.title, "api");
        assert_eq!(res.integration.id, "api");
        // Only the valid POST survived.
        assert_eq!(res.integration.actions.len(), 1);
        assert_eq!(res.integration.actions[0].name, "doPost");
        // Two skip warnings (non-object op, non-object path) + servers warning.
        assert!(res.warnings.iter().any(|w| w.contains("GET /a")));
        assert!(res.warnings.iter().any(|w| w.contains("path '/b'")));
    }

    #[test]
    fn parameter_without_schema_defaults_to_string() {
        let spec = r#"
        {
          "info": { "title": "Defaults" },
          "servers": [ { "url": "https://x" } ],
          "paths": {
            "/q": {
              "get": {
                "operationId": "q",
                "parameters": [ { "name": "term", "in": "query" } ]
              }
            }
          }
        }
        "#;
        let res = generate(spec).expect("generates");
        let q = action(&res, "q");
        assert_eq!(
            q.input_schema["properties"]["term"]["type"],
            json!("string")
        );
        assert_eq!(q.input_schema["properties"]["term"]["x-in"], json!("query"));
    }

    #[test]
    fn access_classification_covers_all_methods() {
        assert_eq!(Access::for_method("get"), Access::Read);
        assert_eq!(Access::for_method("HEAD"), Access::Read);
        assert_eq!(Access::for_method("options"), Access::Read);
        assert_eq!(Access::for_method("post"), Access::Write);
        assert_eq!(Access::for_method("PUT"), Access::Write);
        assert_eq!(Access::for_method("patch"), Access::Write);
        assert_eq!(Access::for_method("delete"), Access::Destructive);
    }

    #[test]
    fn access_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&Access::Read).unwrap(), "\"read\"");
        assert_eq!(serde_json::to_string(&Access::Write).unwrap(), "\"write\"");
        assert_eq!(
            serde_json::to_string(&Access::Destructive).unwrap(),
            "\"destructive\""
        );
    }
}
