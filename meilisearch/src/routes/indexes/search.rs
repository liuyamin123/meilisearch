use actix_web::web::Data;
use actix_web::{web, HttpRequest, HttpResponse};
use index_scheduler::IndexScheduler;
use log::debug;
use meilisearch_auth::IndexSearchRules;
use meilisearch_types::error::ResponseError;
use serde::Deserialize;
use serde_cs::vec::CS;
use serde_json::Value;

use crate::analytics::{Analytics, SearchAggregator};
use crate::extractors::authentication::policies::*;
use crate::extractors::authentication::GuardedData;
use crate::extractors::sequential_extractor::SeqHandler;
use crate::search::{
    perform_search, MatchingStrategy, SearchQuery, DEFAULT_CROP_LENGTH, DEFAULT_CROP_MARKER,
    DEFAULT_HIGHLIGHT_POST_TAG, DEFAULT_HIGHLIGHT_PRE_TAG, DEFAULT_SEARCH_LIMIT,
    DEFAULT_SEARCH_OFFSET,
};
use crate::RateLimiters;

pub fn configure(cfg: &mut web::ServiceConfig, rate_limiters: RateLimiters) {
    cfg.service(
        web::resource("")
            .wrap(rate_limiters.global.into_middleware())
            .wrap(rate_limiters.ip.into_middleware())
            .wrap(rate_limiters.api_key.into_middleware())
            .route(web::get().to(SeqHandler(search_with_url_query)))
            .route(web::post().to(SeqHandler(search_with_post))),
    );
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchQueryGet {
    q: Option<String>,
    #[serde(default = "DEFAULT_SEARCH_OFFSET")]
    offset: usize,
    #[serde(default = "DEFAULT_SEARCH_LIMIT")]
    limit: usize,
    page: Option<usize>,
    hits_per_page: Option<usize>,
    attributes_to_retrieve: Option<CS<String>>,
    attributes_to_crop: Option<CS<String>>,
    #[serde(default = "DEFAULT_CROP_LENGTH")]
    crop_length: usize,
    attributes_to_highlight: Option<CS<String>>,
    filter: Option<String>,
    sort: Option<String>,
    #[serde(default = "Default::default")]
    show_matches_position: bool,
    facets: Option<CS<String>>,
    #[serde(default = "DEFAULT_HIGHLIGHT_PRE_TAG")]
    highlight_pre_tag: String,
    #[serde(default = "DEFAULT_HIGHLIGHT_POST_TAG")]
    highlight_post_tag: String,
    #[serde(default = "DEFAULT_CROP_MARKER")]
    crop_marker: String,
    #[serde(default)]
    matching_strategy: MatchingStrategy,
}

impl From<SearchQueryGet> for SearchQuery {
    fn from(other: SearchQueryGet) -> Self {
        let filter = match other.filter {
            Some(f) => match serde_json::from_str(&f) {
                Ok(v) => Some(v),
                _ => Some(Value::String(f)),
            },
            None => None,
        };

        Self {
            q: other.q,
            offset: other.offset,
            limit: other.limit,
            page: other.page,
            hits_per_page: other.hits_per_page,
            attributes_to_retrieve: other.attributes_to_retrieve.map(|o| o.into_iter().collect()),
            attributes_to_crop: other.attributes_to_crop.map(|o| o.into_iter().collect()),
            crop_length: other.crop_length,
            attributes_to_highlight: other.attributes_to_highlight.map(|o| o.into_iter().collect()),
            filter,
            sort: other.sort.map(|attr| fix_sort_query_parameters(&attr)),
            show_matches_position: other.show_matches_position,
            facets: other.facets.map(|o| o.into_iter().collect()),
            highlight_pre_tag: other.highlight_pre_tag,
            highlight_post_tag: other.highlight_post_tag,
            crop_marker: other.crop_marker,
            matching_strategy: other.matching_strategy,
        }
    }
}

/// Incorporate search rules in search query
fn add_search_rules(query: &mut SearchQuery, rules: IndexSearchRules) {
    query.filter = match (query.filter.take(), rules.filter) {
        (None, rules_filter) => rules_filter,
        (filter, None) => filter,
        (Some(filter), Some(rules_filter)) => {
            let filter = match filter {
                Value::Array(filter) => filter,
                filter => vec![filter],
            };
            let rules_filter = match rules_filter {
                Value::Array(rules_filter) => rules_filter,
                rules_filter => vec![rules_filter],
            };

            Some(Value::Array([filter, rules_filter].concat()))
        }
    }
}

// TODO: TAMO: split on :asc, and :desc, instead of doing some weird things

/// Transform the sort query parameter into something that matches the post expected format.
fn fix_sort_query_parameters(sort_query: &str) -> Vec<String> {
    let mut sort_parameters = Vec::new();
    let mut merge = false;
    for current_sort in sort_query.trim_matches('"').split(',').map(|s| s.trim()) {
        if current_sort.starts_with("_geoPoint(") {
            sort_parameters.push(current_sort.to_string());
            merge = true;
        } else if merge && !sort_parameters.is_empty() {
            let s = sort_parameters.last_mut().unwrap();
            s.push(',');
            s.push_str(current_sort);
            if current_sort.ends_with("):desc") || current_sort.ends_with("):asc") {
                merge = false;
            }
        } else {
            sort_parameters.push(current_sort.to_string());
            merge = false;
        }
    }
    sort_parameters
}

pub async fn search_with_url_query(
    index_scheduler: GuardedData<ActionPolicy<{ actions::SEARCH }>, Data<IndexScheduler>>,
    index_uid: web::Path<String>,
    params: web::Query<SearchQueryGet>,
    req: HttpRequest,
    analytics: web::Data<dyn Analytics>,
) -> Result<HttpResponse, ResponseError> {
    debug!("called with params: {:?}", params);
    let mut query: SearchQuery = params.into_inner().into();

    // Tenant token search_rules.
    if let Some(search_rules) =
        index_scheduler.filters().search_rules.get_index_search_rules(&index_uid)
    {
        add_search_rules(&mut query, search_rules);
    }

    let mut aggregate = SearchAggregator::from_query(&query, &req);

    let index = index_scheduler.index(&index_uid)?;
    let search_result = tokio::task::spawn_blocking(move || perform_search(&index, query)).await?;
    if let Ok(ref search_result) = search_result {
        aggregate.succeed(search_result);
    }
    analytics.get_search(aggregate);

    let search_result = search_result?;

    debug!("returns: {:?}", search_result);
    Ok(HttpResponse::Ok().json(search_result))
}

pub async fn search_with_post(
    index_scheduler: GuardedData<ActionPolicy<{ actions::SEARCH }>, Data<IndexScheduler>>,
    index_uid: web::Path<String>,
    params: web::Json<SearchQuery>,
    req: HttpRequest,
    analytics: web::Data<dyn Analytics>,
) -> Result<HttpResponse, ResponseError> {
    let mut query = params.into_inner();
    debug!("search called with params: {:?}", query);

    // Tenant token search_rules.
    if let Some(search_rules) =
        index_scheduler.filters().search_rules.get_index_search_rules(&index_uid)
    {
        add_search_rules(&mut query, search_rules);
    }

    let mut aggregate = SearchAggregator::from_query(&query, &req);

    let index = index_scheduler.index(&index_uid)?;
    let search_result = tokio::task::spawn_blocking(move || perform_search(&index, query)).await?;
    if let Ok(ref search_result) = search_result {
        aggregate.succeed(search_result);
    }
    analytics.post_search(aggregate);

    let search_result = search_result?;

    debug!("returns: {:?}", search_result);
    Ok(HttpResponse::Ok().json(search_result))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_fix_sort_query_parameters() {
        let sort = fix_sort_query_parameters("_geoPoint(12, 13):asc");
        assert_eq!(sort, vec!["_geoPoint(12,13):asc".to_string()]);
        let sort = fix_sort_query_parameters("doggo:asc,_geoPoint(12.45,13.56):desc");
        assert_eq!(sort, vec!["doggo:asc".to_string(), "_geoPoint(12.45,13.56):desc".to_string(),]);
        let sort = fix_sort_query_parameters(
            "doggo:asc , _geoPoint(12.45, 13.56, 2590352):desc , catto:desc",
        );
        assert_eq!(
            sort,
            vec![
                "doggo:asc".to_string(),
                "_geoPoint(12.45,13.56,2590352):desc".to_string(),
                "catto:desc".to_string(),
            ]
        );
        let sort = fix_sort_query_parameters("doggo:asc , _geoPoint(1, 2), catto:desc");
        // This is ugly but eh, I don't want to write a full parser just for this unused route
        assert_eq!(sort, vec!["doggo:asc".to_string(), "_geoPoint(1,2),catto:desc".to_string(),]);
    }
}
