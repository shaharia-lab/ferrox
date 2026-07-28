use axum::{extract::State, Json};

use crate::state::AppState;
use crate::types::{ModelObject, ModelsResponse};

#[utoipa::path(
    get,
    path = "/v1/models",
    tag = "OpenAI",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Configured model aliases in OpenAI list format", body = ModelsResponse),
        (status = 401, description = "Missing or invalid credentials", body = ErrorResponse),
    )
)]
pub async fn list_models(State(state): State<AppState>) -> Json<ModelsResponse> {
    let data = state
        .config
        .models
        .iter()
        .map(|m| ModelObject {
            id: m.alias.clone(),
            object: "model".to_string(),
            created: 0,
            owned_by: "proxy".to_string(),
        })
        .collect();

    Json(ModelsResponse {
        object: "list".to_string(),
        data,
    })
}
