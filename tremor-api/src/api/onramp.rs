// Copyright 2018-2019, Wayfair GmbH
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::api::*;
use actix_web::{
    error,
    web::{Data, Path},
    HttpRequest,
};
use tremor_runtime::errors::*;

#[derive(Serialize)]
struct OnRampWrap {
    artefact: tremor_runtime::config::OnRamp,
    instances: Vec<String>,
}

pub fn list_artefact((req, data): (HttpRequest, Data<State>)) -> ApiResult {
    let res: Result<Vec<String>> = data.world.repo.list_onramps().map(|l| {
        l.iter()
            .filter_map(tremor_runtime::url::TremorURL::artefact)
            .collect()
    });
    reply(req, data, res, false, 200)
}

pub fn publish_artefact((req, data, data_raw): (HttpRequest, Data<State>, String)) -> ApiResult {
    let decoded_data: tremor_runtime::config::OnRamp = decode(&req, &data_raw)?;
    let url = build_url(&["onramp", &decoded_data.id])?;
    let res = data.world.repo.publish_onramp(url, false, decoded_data);
    reply(req, data, res, true, 201)
}

pub fn unpublish_artefact(
    (req, data, id): (HttpRequest, Data<State>, Path<(String)>),
) -> ApiResult {
    let url = build_url(&["onramp", &id])?;
    let res = data.world.repo.unpublish_onramp(url);
    reply(req, data, res, true, 200)
}

pub fn get_artefact((req, data, id): (HttpRequest, Data<State>, Path<String>)) -> ApiResult {
    let url = build_url(&["onramp", &id])?;
    let res = data
        .world
        .repo
        .find_onramp(url)
        .map_err(|_e| error::ErrorInternalServerError("lookup failed"))?;
    match res {
        Some(res) => {
            let res: Result<OnRampWrap> = Ok(OnRampWrap {
                artefact: res.artefact,
                instances: res
                    .instances
                    .iter()
                    .filter_map(tremor_runtime::url::TremorURL::instance)
                    .collect(),
            });
            reply(req, data, res, false, 200)
        }
        None => Err(error::ErrorNotFound(r#"{"error": "Artefact not found"}"#)),
    }
}