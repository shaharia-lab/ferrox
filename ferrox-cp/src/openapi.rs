//! OpenAPI 3.1 schema for the control plane's REST API.
//!
//! The document is built by `utoipa` from the `#[utoipa::path]` annotations on
//! the handlers plus the `ToSchema`-deriving request/response types, and served
//! from a cold, unauthenticated `/api-schema` (alias `/openapi.json`) route.
//!
//! Unlike the gateway's schema, every body here is a type this crate owns, so
//! the shapes are modeled in full.

use std::sync::LazyLock;

use serde::Serialize;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};

/// The control plane's error envelope: `{"error": ..., "message": ...}`.
///
/// Schema-only: admin handlers and the admin-key middleware build it as a JSON
/// value, and `POST /token` uses `crate::handlers::token::ErrorResponse`; all of
/// them serialize to this shape.
#[derive(Serialize, ToSchema)]
pub struct ApiError {
    /// Short error code: an HTTP status code (e.g. `"404"`, most admin handler
    /// errors) or a category (e.g. `unauthorized`, `internal_error`).
    #[schema(example = "404")]
    pub error: String,
    /// Human-readable error message.
    #[schema(example = "client not found")]
    pub message: String,
}

/// Adds the two authentication schemes the control plane accepts to the spec's
/// components so annotated routes can reference them.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .as_mut()
            .expect("components exist once schemas are registered");
        // Admin routes (`/api/*`): `Authorization: Bearer <CP_ADMIN_KEY>`.
        components.add_security_scheme(
            "admin_auth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("The static admin key configured as `CP_ADMIN_KEY`."))
                    .build(),
            ),
        );
        // Token exchange: `Authorization: Bearer sk-cp-<client API key>`.
        components.add_security_scheme(
            "client_key_auth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "A client API key (`sk-cp-...`) as returned by `POST /api/clients`.",
                    ))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Ferrox Control Plane API",
        description = "Control plane for the Ferrox LLM gateway: API client administration, \
                       usage and budgets, signing-key rotation and the audit log (admin key \
                       required), plus the public token exchange, JWKS and health endpoints.",
    ),
    paths(
        crate::handlers::admin::clients::create_client,
        crate::handlers::admin::clients::list_clients,
        crate::handlers::admin::clients::get_client,
        crate::handlers::admin::clients::revoke_client,
        crate::handlers::admin::clients::client_usage,
        crate::handlers::admin::clients::client_usage_details,
        crate::handlers::admin::clients::update_client_budget,
        crate::handlers::admin::clients::reactivate_client,
        crate::handlers::admin::signing_keys::list_signing_keys,
        crate::handlers::admin::signing_keys::rotate_keys,
        crate::handlers::admin::audit::list_audit,
        crate::handlers::token::token_handler,
        crate::handlers::jwks::jwks_handler,
        crate::handlers::health::health_handler,
    ),
    components(schemas(
        ApiError,
        crate::handlers::admin::clients::CreateClientRequest,
        crate::handlers::admin::clients::CreateClientResponse,
        crate::handlers::admin::clients::ClientResponse,
        crate::handlers::admin::clients::UpdateBudgetRequest,
        crate::handlers::admin::clients::UsageResponse,
        crate::handlers::admin::clients::UsageDetailRecord,
        crate::db::models::UsageSummary,
        crate::handlers::admin::signing_keys::SigningKeyResponse,
        crate::db::models::AuditEntry,
        crate::handlers::token::TokenResponse,
        crate::handlers::jwks::JwksResponse,
        crate::crypto::jwks::Jwk,
        crate::handlers::health::HealthResponse,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "Clients", description = "API client CRUD, usage and budgets (admin key)"),
        (name = "Signing keys", description = "JWT signing-key listing and rotation (admin key)"),
        (name = "Audit", description = "Immutable audit log (admin key)"),
        (name = "Auth", description = "Token exchange and JWKS for the gateway"),
        (name = "Observability", description = "Health (public, unauthenticated)"),
    )
)]
pub struct ApiDoc;

/// The generated OpenAPI document as a pretty-printed JSON string, built once.
/// The `info.version` is stamped from the crate version at build time (utoipa's
/// `info(version=...)` attribute only accepts a string literal, so it's set
/// here instead).
static OPENAPI_JSON: LazyLock<String> = LazyLock::new(|| {
    let mut doc = ApiDoc::openapi();
    doc.info.version = env!("CARGO_PKG_VERSION").to_string();
    doc.to_pretty_json()
        .expect("OpenAPI document serializes to JSON")
});

/// Cached OpenAPI JSON served by the `/api-schema` and `/openapi.json` routes.
pub fn openapi_json() -> &'static str {
    OPENAPI_JSON.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute path to the committed snapshot, read at runtime (not via
    /// `include_str!`) so the crate still compiles before the file is first
    /// generated by `regenerate_committed_snapshot`.
    const SNAPSHOT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json");

    fn doc() -> serde_json::Value {
        serde_json::from_str(openapi_json()).unwrap()
    }

    #[test]
    fn document_is_valid_openapi_3x() {
        let v = doc();
        assert!(
            v["openapi"].as_str().unwrap_or_default().starts_with("3."),
            "openapi version must be 3.x, got {:?}",
            v["openapi"]
        );
        assert!(v["info"]["title"].is_string());
        assert!(v["paths"].is_object());
    }

    #[test]
    fn all_cp_routes_are_enumerated_with_per_route_auth() {
        let v = doc();
        let paths = &v["paths"];
        // (path, method, security scheme — `None` for public routes).
        let routes = [
            ("/api/clients", "post", Some("admin_auth")),
            ("/api/clients", "get", Some("admin_auth")),
            ("/api/clients/{id}", "get", Some("admin_auth")),
            ("/api/clients/{id}", "delete", Some("admin_auth")),
            ("/api/clients/{id}/usage", "get", Some("admin_auth")),
            ("/api/clients/{id}/usage/details", "get", Some("admin_auth")),
            ("/api/clients/{id}/budget", "patch", Some("admin_auth")),
            ("/api/clients/{id}/reactivate", "post", Some("admin_auth")),
            ("/api/signing-keys", "get", Some("admin_auth")),
            ("/api/signing-keys/rotate", "post", Some("admin_auth")),
            ("/api/audit", "get", Some("admin_auth")),
            ("/token", "post", Some("client_key_auth")),
            ("/.well-known/jwks.json", "get", None),
            ("/healthz", "get", None),
        ];
        for (path, method, scheme) in routes {
            let op = &paths[path][method];
            assert!(
                op.is_object(),
                "{} {path} missing from schema",
                method.to_uppercase()
            );
            let security = &op["security"];
            match scheme {
                Some(s) => assert!(
                    security[0].get(s).is_some(),
                    "{} {path} must require {s}, got {security}",
                    method.to_uppercase()
                ),
                None => assert!(
                    security.is_null(),
                    "{} {path} must be public, got {security}",
                    method.to_uppercase()
                ),
            }
        }
        let documented: usize = paths
            .as_object()
            .unwrap()
            .values()
            .map(|item| item.as_object().unwrap().len())
            .sum();
        assert_eq!(
            documented,
            routes.len(),
            "undocumented or unexpected operations"
        );
    }

    #[test]
    fn owned_shapes_and_security_are_modeled() {
        let v = doc();
        let schemas = &v["components"]["schemas"];
        for s in [
            "ApiError",
            "CreateClientRequest",
            "CreateClientResponse",
            "ClientResponse",
            "UpdateBudgetRequest",
            "UsageResponse",
            "UsageSummary",
            "UsageDetailRecord",
            "SigningKeyResponse",
            "AuditEntry",
            "TokenResponse",
            "JwksResponse",
            "Jwk",
            "HealthResponse",
        ] {
            assert!(schemas.get(s).is_some(), "schema {s} missing");
        }
        // Key material is never part of a client response.
        let client = &schemas["ClientResponse"]["properties"];
        assert!(client.get("api_key").is_none());
        assert!(client.get("api_key_hash").is_none());
        assert!(schemas["CreateClientResponse"]["properties"]
            .get("api_key")
            .is_some());
        // Serde renames are honored (`key_use` serializes as `use`).
        assert!(schemas["Jwk"]["properties"].get("use").is_some());
        // Both auth schemes present.
        let sec = &v["components"]["securitySchemes"];
        assert!(sec["admin_auth"].is_object());
        assert!(sec["client_key_auth"].is_object());
    }

    #[test]
    fn every_ref_resolves_to_a_registered_schema() {
        // No dangling `$ref`: referencing a schema by full path instead of its
        // registered component name silently produces an unresolvable ref.
        let v = doc();
        let schemas = &v["components"]["schemas"];

        fn collect_refs<'a>(node: &'a serde_json::Value, out: &mut Vec<&'a str>) {
            match node {
                serde_json::Value::Object(map) => {
                    for (k, val) in map {
                        if k == "$ref" {
                            if let Some(s) = val.as_str() {
                                out.push(s);
                            }
                        } else {
                            collect_refs(val, out);
                        }
                    }
                }
                serde_json::Value::Array(arr) => arr.iter().for_each(|x| collect_refs(x, out)),
                _ => {}
            }
        }

        let mut refs = Vec::new();
        collect_refs(&v, &mut refs);
        assert!(
            !refs.is_empty(),
            "expected at least one $ref in the document"
        );
        for r in refs {
            let name = r
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected $ref form: {r}"));
            assert!(
                schemas.get(name).is_some(),
                "dangling $ref {r}: no component schema named {name:?}"
            );
        }
    }

    /// Drift guard: the committed `ferrox-cp/openapi.json` must match the spec
    /// generated from the code. Regenerate with:
    /// `cargo test -p ferrox-cp openapi::tests::regenerate_committed_snapshot -- --ignored`
    #[test]
    fn committed_snapshot_matches_generated() {
        let committed = std::fs::read_to_string(SNAPSHOT_PATH).unwrap_or_else(|e| {
            panic!("committed snapshot {SNAPSHOT_PATH} unreadable ({e}); generate it with the ignored `regenerate_committed_snapshot` test")
        });
        assert_eq!(
            openapi_json().trim(),
            committed.trim(),
            "ferrox-cp/openapi.json is out of date — regenerate with the ignored \
             `regenerate_committed_snapshot` test and commit it"
        );
    }

    /// Writes the current spec to the committed snapshot. Ignored by default so
    /// it never runs in CI; invoke explicitly to (re)generate the file.
    #[test]
    #[ignore = "regeneration utility, run explicitly"]
    fn regenerate_committed_snapshot() {
        std::fs::write(SNAPSHOT_PATH, format!("{}\n", openapi_json())).unwrap();
    }
}
