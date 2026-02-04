// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use axum::routing::get;
use axum::{Json, Router};
use http::StatusCode;
use kube::core::Status;
use kube::core::conversion::{ConversionRequest, ConversionResponse, ConversionReview};
use kube::core::response::reason;

use mz_cloud_resources::crd::materialize::{v1alpha1, v1alpha2};

pub fn router() -> Router {
    Router::new().route("/convert", get(get_convert))
}

fn invalid_response(message: &str) -> (StatusCode, Json<ConversionReview>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(ConversionResponse::invalid(Status::failure(message, reason::INVALID)).into_review()),
    )
}

async fn get_convert(
    Json(conversion_review): Json<ConversionReview>,
) -> (StatusCode, Json<ConversionReview>) {
    if &conversion_review.types.api_version != "materialize.cloud/v1alpha1" {
        return invalid_response(&format!(
            "expected api_version materialize.cloud/v1alpha1, but got {}",
            conversion_review.types.api_version
        ));
    }
    if &conversion_review.types.kind != "Materialize" {
        return invalid_response(&format!(
            "expected kind Materialize, but got {}",
            conversion_review.types.kind
        ));
    }
    let Ok(request) = ConversionRequest::from_review(conversion_review) else {
        return invalid_response("missing request");
    };

    let converted_objects: Result<Vec<serde_json::Value>, serde_json::Error> = request
        .objects
        .iter()
        .cloned()
        .map(|value| {
            serde_json::from_value::<v1alpha1::Materialize>(value).and_then(|mz_v1alpha1| {
                serde_json::to_value(v1alpha2::Materialize::from(mz_v1alpha1))
            })
        })
        .collect();
    match converted_objects {
        Ok(converted_objects) => (
            StatusCode::OK,
            Json(
                ConversionResponse::for_request(request)
                    .success(converted_objects)
                    .into_review(),
            ),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                ConversionResponse::for_request(request)
                    .failure(Status::failure(&e.to_string(), reason::UNKNOWN))
                    .into_review(),
            ),
        ),
    }
}
