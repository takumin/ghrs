//! Thin wrapper around the GitHub REST API. Counts the number of calls.

use anyhow::{Context, Result, bail};
use octocrab::Octocrab;
use serde_json::Value as Json;
use std::future::Future;

/// Number of items per page for list APIs. GitHub's maximum.
const PER_PAGE: usize = 100;

/// Maximum number of pages to fetch. A safeguard against fetching forever even if the
/// response is unexpected.
const MAX_PAGES: u32 = 100;

/// Raw write requests do not resolve relative paths, so send absolute URIs.
/// Only github.com is targeted (GHES is out of scope).
const API: &str = "https://api.github.com";

#[derive(Clone, Copy)]
pub enum Method {
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

pub struct GitHub {
    octo: Octocrab,
    pub calls: u32,
}

impl GitHub {
    pub fn new(octo: Octocrab) -> Self {
        Self { octo, calls: 0 }
    }

    pub async fn get(&mut self, route: &str) -> Result<Json> {
        self.calls += 1;
        self.octo
            .get(route, None::<&()>)
            .await
            .with_context(|| format!("GET {route}"))
    }

    /// Fetch all pages of a list API. Each page counts as one call.
    ///
    /// octocrab's `get` does not return the `Link` header, so a page with fewer items than
    /// `PER_PAGE` is treated as the last one. If the item count is an exact multiple of
    /// `PER_PAGE`, one extra empty page is fetched at the end.
    pub async fn get_all(&mut self, route: &str) -> Result<Vec<Json>> {
        let separator = if route.contains('?') { '&' } else { '?' };
        let octo = &self.octo;
        let calls = &mut self.calls;
        paginate(|page| {
            *calls += 1;
            let route = format!("{route}{separator}per_page={PER_PAGE}&page={page}");
            async move {
                octo.get(&route, None::<&()>)
                    .await
                    .with_context(|| format!("GET {route}"))
            }
        })
        .await
        .with_context(|| format!("GET {route}"))
    }

    /// Like [`Self::get_all`], for list endpoints that wrap each page in an object
    /// (`{"total_count": 1, "<key>": [...]}`). Returns 404 as `None`.
    pub async fn get_all_wrapped(&mut self, route: &str, key: &str) -> Result<Option<Vec<Json>>> {
        let separator = if route.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{route}{separator}per_page={PER_PAGE}&page={page}");
            let Some(mut v) = self.get_optional(&url).await? else {
                return Ok(None);
            };
            let Json::Array(chunk) = v[key].take() else {
                bail!("response page {page} of GET {route} has no {key} array");
            };
            let last = chunk.len() < PER_PAGE;
            items.extend(chunk);
            if last {
                return Ok(Some(items));
            }
        }
        bail!("stopped fetching after {MAX_PAGES} pages")
    }

    /// Return 404 as `None`. Used for calls that double as existence checks.
    pub async fn get_optional(&mut self, route: &str) -> Result<Option<Json>> {
        self.calls += 1;
        match self.octo.get(route, None::<&()>).await {
            Ok(v) => Ok(Some(v)),
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 404 => {
                Ok(None)
            }
            Err(err) => Err(err).with_context(|| format!("GET {route}")),
        }
    }

    /// Core rate limit (remaining, limit). `GET /rate_limit` does not consume the rate limit, so
    /// it is not counted as a call.
    pub async fn rate_limit(&self) -> Result<(u64, u64)> {
        let v: Json = self
            .octo
            .get("/rate_limit", None::<&()>)
            .await
            .context("GET /rate_limit")?;
        let core = &v["resources"]["core"];
        match (core["remaining"].as_u64(), core["limit"].as_u64()) {
            (Some(remaining), Some(limit)) => Ok((remaining, limit)),
            _ => anyhow::bail!("response of GET /rate_limit has no core"),
        }
    }

    /// Send a write. Returns `Json::Null` if the response is empty (204).
    pub async fn send(&mut self, method: Method, route: &str, body: Option<&Json>) -> Result<Json> {
        self.calls += 1;
        let uri = format!("{API}{route}");
        let response = match method {
            Method::Post => self.octo._post(uri, body).await,
            Method::Put => self.octo._put(uri, body).await,
            Method::Patch => self.octo._patch(uri, body).await,
            Method::Delete => self.octo._delete(uri, body).await,
        };
        let response = response.with_context(|| format!("{} {route}", method.as_str()))?;
        let status = response.status();

        let text =
            self.octo.body_to_string(response).await.with_context(|| {
                format!("cannot read the response of {} {route}", method.as_str())
            })?;
        if !status.is_success() {
            bail!(
                "{} {route}: {}",
                method.as_str(),
                describe(status.as_u16(), &text)
            );
        }
        if text.trim().is_empty() {
            return Ok(Json::Null);
        }
        serde_json::from_str(&text)
            .with_context(|| format!("response of {} {route} is not JSON", method.as_str()))
    }
}

/// Fetch pages in order starting from 1 and concatenate their items. Kept separate from HTTP
/// so it can be tested.
async fn paginate<F, Fut>(mut fetch: F) -> Result<Vec<Json>>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<Json>>,
{
    let mut items = Vec::new();
    for page in 1..=MAX_PAGES {
        let Json::Array(chunk) = fetch(page).await? else {
            bail!("response page {page} is not an array");
        };
        let last = chunk.len() < PER_PAGE;
        items.extend(chunk);
        if last {
            return Ok(items);
        }
    }
    bail!("stopped fetching after {MAX_PAGES} pages")
}

/// Turn the body of an error response into a single message.
///
/// The reason for a 422 is in `errors`, but its shape is not consistent: it may be an array of
/// objects, an array of strings, or a single string (observed: exceeding the ruleset limit gives
/// a single string). octocrab only understands arrays and fails with a Serde error on a single
/// string, so we read the body ourselves.
fn describe(status: u16, body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Json>(body) else {
        return format!("HTTP {status}: {}", body.trim());
    };
    let mut message = format!(
        "{} (HTTP {status})",
        v["message"].as_str().unwrap_or("error")
    );
    let details: Vec<&Json> = match &v["errors"] {
        Json::Array(items) => items.iter().collect(),
        Json::Null => Vec::new(),
        other => vec![other],
    };
    for detail in details {
        let detail = match (detail, detail["message"].as_str()) {
            (Json::String(s), _) => s.clone(),
            (_, Some(text)) => match detail["field"].as_str() {
                Some(field) => format!("{field}: {text}"),
                None => text.to_string(),
            },
            (other, None) => other.to_string(),
        };
        message.push_str(&format!("\n  - {detail}"));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fake API returning `total` items `PER_PAGE` at a time. Records the requested page numbers.
    async fn fetch_all(total: usize) -> (Result<Vec<Json>>, Vec<u32>) {
        let mut requested = Vec::new();
        let result = paginate(|page| {
            requested.push(page);
            let start = (page as usize - 1) * PER_PAGE;
            let chunk: Vec<Json> = (start..total.min(start + PER_PAGE))
                .map(|i| json!(i))
                .collect();
            async move { Ok(Json::Array(chunk)) }
        })
        .await;
        (result, requested)
    }

    #[test]
    fn describes_every_shape_of_errors() {
        let body =
            r#"{"message":"Validation Failed","errors":"The ruleset limit has been reached."}"#;
        assert_eq!(
            describe(422, body),
            "Validation Failed (HTTP 422)\n  - The ruleset limit has been reached."
        );

        let body = concat!(
            r#"{"message":"Validation Failed","errors":"#,
            r#"[{"field":"default_branch","message":"not found"},"plain"]}"#,
        );
        assert_eq!(
            describe(422, body),
            "Validation Failed (HTTP 422)\n  - default_branch: not found\n  - plain"
        );

        assert_eq!(
            describe(404, r#"{"message":"Not Found"}"#),
            "Not Found (HTTP 404)"
        );
        assert_eq!(describe(502, "Bad Gateway"), "HTTP 502: Bad Gateway");
    }

    #[tokio::test]
    async fn collects_every_page() {
        for (total, pages) in [(0, 1), (1, 1), (99, 1), (100, 2), (101, 2), (250, 3)] {
            let (result, requested) = fetch_all(total).await;
            let items = result.unwrap();
            assert_eq!(items.len(), total, "total = {total}");
            assert_eq!(
                items.last(),
                total.checked_sub(1).map(|i| json!(i)).as_ref()
            );
            assert_eq!(
                requested,
                (1..=pages).collect::<Vec<_>>(),
                "total = {total}"
            );
        }
    }

    #[tokio::test]
    async fn stops_after_max_pages() {
        let result = paginate(|_| async { Ok(Json::Array(vec![json!(0); PER_PAGE])) }).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_non_array_pages() {
        let result = paginate(|_| async { Ok(json!({ "message": "x" })) }).await;
        assert!(result.is_err());
    }
}
