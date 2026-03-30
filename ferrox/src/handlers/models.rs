use axum::{extract::State, Json};

use crate::state::AppState;
use crate::types::{ModelObject, ModelsResponse};

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
