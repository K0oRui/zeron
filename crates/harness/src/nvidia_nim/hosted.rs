//! NVIDIA's public "Free Endpoint" catalog is separate from `/v1/models`.
//! Read metadata only: never infer free access from a model name or send a probe.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;

pub(super) const SEARCH_URL: &str =
    "https://api.ngc.nvidia.com/v2/search/catalog/resources/ENDPOINT";
pub(super) const SPECS_URL: &str = "https://api.ngc.nvidia.com/v2/endpoints";
pub(super) const PAGE_SIZE: usize = 100;
pub(super) const MAX_PAGES: usize = 20;
pub(super) const SPEC_CONCURRENCY: usize = 4;

#[derive(Clone)]
pub(super) struct Catalog {
    pub search_url: String,
    pub specs_url: String,
}

impl Catalog {
    pub fn public() -> Self {
        Self {
            search_url: SEARCH_URL.into(),
            specs_url: SPECS_URL.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResponse {
    params: SearchParams,
    result_total: usize,
    results: Vec<Group>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchParams {
    page: usize,
    page_size: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Group {
    group_value: String,
    total_count: usize,
    resources: Vec<Value>,
}

pub(super) struct Page {
    pub total: usize,
    pub count: usize,
    pub resources: Vec<Resource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Resource {
    name: String,
    org_name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    attributes: Vec<Attribute>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Label {
    key: String,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    unresolved_values: Vec<String>,
}

#[derive(Deserialize)]
struct Attribute {
    key: String,
    value: String,
}

/// The search response also contains a duplicate `_scored` group. Validate the
/// actual ENDPOINT page, including pagination, before accepting any free marks.
pub(super) fn page(value: Value, expected: usize) -> Result<Page, &'static str> {
    let response: SearchResponse = serde_json::from_value(value)
        .map_err(|_| "NVIDIA returned an invalid free-endpoint catalog.")?;
    if response.params.page != expected || response.params.page_size != PAGE_SIZE {
        return Err("NVIDIA returned an unexpected free-endpoint catalog page.");
    }
    let mut groups = response
        .results
        .into_iter()
        .filter(|group| group.group_value == "ENDPOINT");
    let group = groups
        .next()
        .ok_or("NVIDIA omitted the free-endpoint catalog results.")?;
    let count = group.resources.len();
    if groups.next().is_some()
        || group.total_count != response.result_total
        || response.result_total > PAGE_SIZE * MAX_PAGES
        || expected * PAGE_SIZE + count > response.result_total
        || count != PAGE_SIZE.min(response.result_total.saturating_sub(expected * PAGE_SIZE))
    {
        return Err("NVIDIA returned an incomplete free-endpoint catalog page.");
    }
    Ok(Page {
        total: response.result_total,
        count,
        resources: group
            .resources
            .into_iter()
            .filter_map(|row| serde_json::from_value::<Resource>(row).ok())
            .filter(Resource::is_free)
            .collect(),
    })
}

impl Resource {
    fn is_free(&self) -> bool {
        let marked_free = self.labels.iter().any(|label| {
            label.key == "general"
                && (label.values.iter().any(|value| value == "Free Endpoint")
                    || label
                        .unresolved_values
                        .iter()
                        .any(|value| value == "nim_type_preview"))
        });
        // AVAILABLE describes the downloadable NIM, not the free API. Do not
        // reject hosted-only models just because that attribute is false.
        marked_free
            && !self.attributes.iter().any(|attribute| {
                attribute.key == "PREVIEW" && attribute.value.eq_ignore_ascii_case("false")
            })
    }

    fn publisher(&self) -> Option<&str> {
        let label = self.labels.iter().find(|label| label.key == "publisher")?;
        let value = label
            .unresolved_values
            .first()
            .or_else(|| label.values.first())?;
        valid_segment(value).then_some(value.as_str())
    }

    pub fn location(&self) -> Option<(&str, &str)> {
        (valid_segment(&self.org_name) && valid_segment(&self.name))
            .then_some((&self.org_name, &self.name))
    }

    /// Publisher matching only reduces public metadata requests. Canonical
    /// wire IDs, including aliases unlike the card slug, come from its spec.
    pub fn could_describe(&self, id: &str) -> bool {
        id.split_once('/').map(|(publisher, _)| publisher) == self.publisher()
    }

    pub fn model_ids(&self, spec: &Value) -> HashSet<String> {
        self.declared_models(spec).unwrap_or_default()
    }

    #[cfg(test)]
    fn permits(&self, id: &str, spec: &Value) -> bool {
        self.model_ids(spec).contains(id)
    }

    /// Namespace is an NGC organization, NOT the model vendor. Only actual
    /// public chat specs can establish the wire IDs of a Free Endpoint.
    fn declared_models(&self, spec: &Value) -> Option<HashSet<String>> {
        if !self.is_free() {
            return None;
        }
        if spec.get("artifactName").and_then(Value::as_str) != Some(self.name.as_str())
            || !spec
                .get("nvcfFunctionId")
                .and_then(Value::as_str)
                .is_some_and(|id| {
                    !id.trim().is_empty()
                        && !id.eq_ignore_ascii_case("none")
                        && !id.eq_ignore_ascii_case("null")
                })
            || !matches!(
                spec.get("nvcfInferencePath").and_then(Value::as_str),
                Some("/v1/chat/completions" | "/chat/completions")
            )
        {
            return None;
        }
        let attributes = match json_field(spec.get("attributes")) {
            Some(attributes) => attributes,
            None => return None,
        };
        if attributes
            .get("showUnavailableBanner")
            .and_then(Value::as_bool)
            == Some(true)
        {
            return None;
        }
        let Some(api) = json_field(spec.get("openAPISpec")) else {
            return None;
        };
        if !api
            .get("servers")
            .and_then(Value::as_array)
            .is_some_and(|servers| {
                servers.iter().any(|server| {
                    server
                        .get("url")
                        .and_then(Value::as_str)
                        .is_some_and(super::discovery::is_public_hosted)
                })
            })
        {
            return None;
        }
        let Some(paths) = api.get("paths").and_then(Value::as_object) else {
            return None;
        };
        let mut declared = HashSet::new();
        let mut chat = false;
        for (path, operations) in paths {
            if !matches!(path.as_str(), "/chat/completions" | "/v1/chat/completions") {
                continue;
            }
            let Some(post) = operations.get("post").filter(|value| value.is_object()) else {
                continue;
            };
            chat = true;
            if let Some(examples) = post
                .pointer("/x-nvai-meta/examples")
                .and_then(Value::as_array)
            {
                for example in examples {
                    if let Some(request) = json_field(example.get("requestJson"))
                        && let Some(model) = request.get("model").and_then(Value::as_str)
                    {
                        declared.insert(model.to_owned());
                    }
                }
            }
            if let Some(schema) = post.pointer("/requestBody/content/application~1json/schema")
                && let Some(schema) = resolve(schema, &api)
                && let Some(model) = schema.pointer("/properties/model")
                && let Some(model) = resolve(model, &api)
            {
                for field in ["default", "example", "const"] {
                    if let Some(id) = model.get(field).and_then(Value::as_str) {
                        declared.insert(id.to_owned());
                    }
                }
                if let Some(ids) = model.get("enum").and_then(Value::as_array) {
                    declared.extend(ids.iter().filter_map(Value::as_str).map(str::to_owned));
                }
            }
        }
        // When the spec omits example/default IDs, accept only the literal
        // publisher/name match. No fuzzy alias or cross-publisher free grants.
        if !chat {
            return None;
        }
        declared.retain(|id| id.contains('/'));
        if declared.is_empty() {
            declared.insert(format!("{}/{}", self.publisher()?, self.name));
        }
        declared
            .retain(|id| id.split_once('/').map(|(publisher, _)| publisher) == self.publisher());
        Some(declared)
    }

    pub fn label(&self) -> Option<&str> {
        (!self.display_name.trim().is_empty()).then_some(self.display_name.trim())
    }
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn json_field(value: Option<&Value>) -> Option<Value> {
    match value? {
        Value::String(text) => serde_json::from_str(text).ok(),
        value @ Value::Object(_) => Some(value.clone()),
        _ => None,
    }
}

fn resolve<'a>(mut schema: &'a Value, api: &'a Value) -> Option<&'a Value> {
    for _ in 0..8 {
        match schema.get("$ref").and_then(Value::as_str) {
            Some(reference) => schema = api.pointer(reference.strip_prefix('#')?)?,
            None => return Some(schema),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resource() -> Resource {
        serde_json::from_value(json!({
            "name":"model-5-3", "orgName":"public-org", "displayName":"Model 5.3",
            "labels":[
                {"key":"general","values":["Free Endpoint","Chat"]},
                {"key":"publisher","values":["vendor"]}
            ],
            "attributes":[{"key":"AVAILABLE","value":"false"}]
        }))
        .unwrap()
    }

    fn spec() -> Value {
        json!({
            "artifactName":"model-5-3", "nvcfFunctionId":"fixture-function",
            "nvcfInferencePath":"/v1/chat/completions",
            "attributes":json!({"showUnavailableBanner":false}).to_string(),
            "openAPISpec":json!({
                "servers":[{"url":"https://integrate.api.nvidia.com/v1/"}],
                "paths":{"/chat/completions":{"post":{
                    "x-nvai-meta":{"examples":[{"requestJson":json!({"model":"vendor/model-5.3"}).to_string()}]}
                }}}
            }).to_string()
        })
    }

    #[test]
    fn a_free_chat_spec_permits_the_exact_advertised_wire_id_not_a_rewrite() {
        let resource = resource();
        assert!(resource.permits("vendor/model-5.3", &spec()));
        assert!(!resource.permits("vendor/model-5-3", &spec()));
        assert!(!resource.permits("another-vendor/model-5.3", &spec()));
        assert!(!resource.permits("vendor/model-5.3-paid", &spec()));
        assert_eq!(resource.label(), Some("Model 5.3"));
    }

    #[test]
    fn marketing_slugs_do_not_hide_a_different_explicit_wire_id() {
        let mut resource = resource();
        resource.name = "a-longer-marketing-slug-70b".into();
        let mut spec = spec();
        spec["artifactName"] = json!("a-longer-marketing-slug-70b");
        assert!(resource.could_describe("vendor/model-5.3"));
        assert!(resource.permits("vendor/model-5.3", &spec));
        assert!(!resource.permits("vendor/a-longer-marketing-slug-70b", &spec));
    }

    #[test]
    fn downloadable_and_partner_labels_do_not_establish_free_inference() {
        let mut resource = resource();
        resource.labels[0].values = vec!["Download Available".into(), "Partner Endpoint".into()];
        assert!(!resource.permits("vendor/model-5.3", &spec()));
        resource.labels[0].unresolved_values = vec!["nim_type_preview".into()];
        assert!(resource.permits("vendor/model-5.3", &spec()));
        resource.attributes.push(Attribute {
            key: "PREVIEW".into(),
            value: "false".into(),
        });
        assert!(!resource.permits("vendor/model-5.3", &spec()));
    }

    #[test]
    fn missing_deployments_other_protocols_and_unavailable_specs_are_not_offered() {
        for (field, value) in [
            ("nvcfFunctionId", Value::Null),
            ("nvcfFunctionId", json!("None")),
            ("nvcfFunctionId", json!("")),
            ("nvcfInferencePath", json!("/v1/embeddings")),
            ("nvcfInferencePath", json!("/vlm/infer")),
            ("artifactName", json!("different-model")),
            ("attributes", json!({"showUnavailableBanner":true})),
            ("openAPISpec", json!("not json")),
            ("attributes", json!("not json")),
        ] {
            let mut spec = spec();
            spec[field] = value;
            assert!(!resource().permits("vendor/model-5.3", &spec), "{field}");
        }
    }

    #[test]
    fn a_partner_server_or_non_chat_openapi_is_not_a_free_public_nim_route() {
        let mut spec = spec();
        let mut api = json_field(spec.get("openAPISpec")).unwrap();
        api["servers"] = json!([{"url":"https://paid.partner.example/v1"}]);
        spec["openAPISpec"] = api.clone();
        assert!(!resource().permits("vendor/model-5.3", &spec));
        api["servers"] = json!([{"url":"https://integrate.api.nvidia.com/v1"}]);
        api["paths"] = json!({"/embeddings":{"post":{}}});
        spec["openAPISpec"] = api;
        assert!(!resource().permits("vendor/model-5.3", &spec));
    }

    #[test]
    fn schema_refs_and_model_enums_are_read_without_following_external_refs() {
        let mut spec = spec();
        spec["openAPISpec"] = json!({
            "servers":[{"url":"https://integrate.api.nvidia.com/v1"}],
            "paths":{"/chat/completions":{"post":{"requestBody":{"content":{"application/json":{
                "schema":{"$ref":"#/components/schemas/Request"}
            }}}}}},
            "components":{"schemas":{
                "Request":{"properties":{"model":{"$ref":"#/components/schemas/Model"}}},
                "Model":{"enum":["vendor/model-5.3"]}
            }}
        });
        assert!(resource().permits("vendor/model-5.3", &spec));
        spec["openAPISpec"]["components"]["schemas"]["Model"] =
            json!({"$ref":"https://untrusted.example/schema"});
        assert!(!resource().permits("vendor/model-5.3", &spec));
    }

    #[test]
    fn an_unspecified_model_is_only_matched_literally_not_by_similar_names() {
        let mut spec = spec();
        spec["openAPISpec"] = json!({
            "servers":[{"url":"https://integrate.api.nvidia.com/v1"}],
            "paths":{"/chat/completions":{"post":{}}}
        });
        assert!(resource().permits("vendor/model-5-3", &spec));
        assert!(!resource().permits("vendor/model-5.3", &spec));
    }

    #[test]
    fn metadata_cannot_escape_the_public_spec_path() {
        for name in [
            "..",
            ".",
            "../private",
            "https://other.example",
            "a?b=c",
            "a#b",
        ] {
            let mut resource = resource();
            resource.name = name.into();
            assert!(resource.location().is_none());
        }
    }

    #[test]
    fn pagination_uses_only_real_endpoint_rows_and_requires_the_full_page() {
        let row = json!({"name":"new-chat", "orgName":"org", "labels":[
            {"key":"general","unresolvedValues":["nim_type_preview"]}
        ]});
        let value = json!({"params":{"page":0,"pageSize":100}, "resultTotal":1,"results":[
            {"groupValue":"_scored","totalCount":1,"resources":[row.clone()]},
            {"groupValue":"ENDPOINT","totalCount":1,"resources":[row]}
        ]});
        let parsed = page(value.clone(), 0).unwrap();
        assert_eq!(
            (parsed.count, parsed.total, parsed.resources.len()),
            (1, 1, 1)
        );
        assert!(page(value.clone(), 1).is_err());
        let mut incomplete = value.clone();
        incomplete["resultTotal"] = json!(101);
        incomplete["results"][1]["totalCount"] = json!(101);
        assert!(page(incomplete, 0).is_err());
        assert!(page(json!({"results":[]}), 0).is_err());
        let mut missing = value;
        missing["results"] = json!([]);
        assert!(page(missing, 0).is_err());
    }

    #[test]
    fn full_and_partial_pages_use_unfiltered_resource_counts() {
        for (index, count) in [(0, 100), (1, 1)] {
            let value = json!({
                "params":{"page":index,"pageSize":100}, "resultTotal":101,
                "results":[{"groupValue":"ENDPOINT","totalCount":101,"resources":vec![Value::Null; count]}]
            });
            let parsed = page(value, index).unwrap();
            assert_eq!(parsed.count, count);
            assert_eq!(parsed.total, 101);
            assert!(parsed.resources.is_empty());
        }
    }

    #[test]
    fn new_free_rows_do_not_need_to_be_featured_or_in_a_builtin_list() {
        let mut resource = resource();
        resource.name = "future-chat".into();
        let mut spec = spec();
        spec["artifactName"] = json!("future-chat");
        let mut api = json_field(spec.get("openAPISpec")).unwrap();
        api["paths"]["/chat/completions"]["post"] = json!({});
        spec["openAPISpec"] = api;
        assert!(resource.permits("vendor/future-chat", &spec));
    }
}
