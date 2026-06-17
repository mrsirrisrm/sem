//! Parkable cross-repo endpoint join (TS SWR/fetch clients <-> JAX-RS API).
//!
//! Companion to [`super::parkable_routes`]. Where that overlay links async task
//! producers to handlers *within* the API repo, this module links the TS client
//! repos (web panel, mobile) to the API repo by matching client request URLs
//! against JAX-RS handler routes. It is a **manifest join**, not a graph
//! mutation: each repo is parsed into its own [`EntityGraph`], and this module
//! extracts two flat manifests — server endpoints and client call sites — that
//! the `sem xref` command joins on (path, verb).
//!
//! Parkable conventions it is keyed to:
//!   * The web/mobile axios base URL is `<host>/api/`, and client URLs are
//!     written relative to it (`v2/parks/${id}`), so the server path is
//!     `/api/` + clientURL.
//!   * Endpoints are versioned by class (`ParksResource2` -> `/api/v2/parks`).
//!   * Reads use SWR (`useMySWR(key, get)`, GET by default); mutations are
//!     direct `post`/`put`/`patch`/`del` calls whose first arg is the URL.

use std::collections::HashMap;
use std::path::Path;

use regex::Regex;

use super::graph::EntityGraph;
use super::parkable_routes::{collect_handler_routes, routes_match, to_segments, Seg};

/// A JAX-RS endpoint exposed by the API repo.
#[derive(Debug, Clone)]
pub struct XrefEndpoint {
    pub entity_id: String,
    pub name: String,
    pub file: String,
    pub line: usize,
    pub verb: String,
    /// Display form of the route, e.g. `/api/v2/parks/*`.
    pub route: String,
    segs: Vec<Seg>,
}

/// A client-side API request site (an SWR hook, key builder, or direct call).
#[derive(Debug, Clone)]
pub struct XrefClientCall {
    pub entity_id: String,
    pub name: String,
    pub file: String,
    pub line: usize,
    pub verb: String,
    /// The URL as written in the client, e.g. `v2/parks/${parkId}`.
    pub url: String,
    segs: Vec<Seg>,
}

/// Collect every JAX-RS endpoint in `graph` (only the API repo yields any).
pub fn collect_endpoints(graph: &EntityGraph, root: &Path) -> Vec<XrefEndpoint> {
    let mut cache: HashMap<String, Option<Vec<String>>> = HashMap::new();
    collect_handler_routes(graph, root, &mut cache)
        .into_iter()
        .map(|h| XrefEndpoint {
            entity_id: h.id,
            name: h.name,
            file: h.file,
            line: h.line,
            verb: h.verb,
            route: render_route(&h.segs),
            segs: h.segs,
        })
        .collect()
}

/// Collect every client API call site in `graph` (only TS repos yield any).
///
/// We scan each file once for URL literals shaped like a versioned API path
/// (`v<n>/...`), optionally with a leading `/api/`, and attribute each to the
/// innermost entity whose span contains it. The HTTP verb is the wrapping
/// `get`/`post`/`put`/`patch`/`del(` call when present, else GET (the SWR
/// default fetcher).
pub fn collect_client_calls(graph: &EntityGraph, root: &Path) -> Vec<XrefClientCall> {
    // group1 = optional verb call; group2 = the `v<n>/...` path (stops at `?`,
    // quote, or whitespace). String or template-literal quoting.
    let re = Regex::new(
        r#"(?:\b(get|post|put|patch|del)\s*(?:<[^>]*>)?\s*\(\s*)?[`"']\s*(?:/?(?:api/)?)(v\d+/[^`"'?\s]*)"#,
    )
    .unwrap();

    // Index entities by file so each URL can be attributed to its innermost
    // containing entity.
    let mut by_file: HashMap<&str, Vec<&super::graph::EntityInfo>> = HashMap::new();
    for ent in graph.entities.values() {
        by_file.entry(ent.file_path.as_str()).or_default().push(ent);
    }

    let mut calls = Vec::new();
    let mut file_cache: HashMap<String, Option<Vec<String>>> = HashMap::new();
    for (file, ents) in &by_file {
        // Only bother reading files that actually have at least one entity.
        let Some(lines) = read_file(&mut file_cache, root, file) else {
            continue;
        };
        let content = lines.join("\n");
        if !content.contains('/') {
            continue;
        }
        for cap in re.captures_iter(&content) {
            let url = cap[2].to_string();
            let verb = match cap.get(1).map(|m| m.as_str()) {
                Some("del") => "DELETE".to_string(),
                Some(v) => v.to_uppercase(),
                None => "GET".to_string(),
            };
            let byte = cap.get(2).unwrap().start();
            let line = content[..byte].bytes().filter(|&b| b == b'\n').count() + 1;
            let Some(owner) = innermost_entity(ents, line) else {
                continue;
            };
            let segs = to_segments(&format!("/api/{url}"), |s| s.contains('{'));
            calls.push(XrefClientCall {
                entity_id: owner.id.clone(),
                name: owner.name.clone(),
                file: owner.file_path.clone(),
                line,
                verb,
                url,
                segs,
            });
        }
    }
    calls
}

/// Does this client call hit this endpoint? Verb must match exactly; the path
/// match is asymmetric with the *endpoint* as the pattern (its `{id}` segments
/// match anything, but its literal segments must be present in the client URL).
pub fn endpoint_matches_call(ep: &XrefEndpoint, call: &XrefClientCall) -> bool {
    ep.verb == call.verb && routes_match(&ep.segs, &call.segs)
}

/// Among the endpoints a call matches, the most specific (most literal
/// segments) wins — mirrors the task-overlay tiebreak.
pub fn best_endpoint<'a>(
    endpoints: &'a [XrefEndpoint],
    call: &XrefClientCall,
) -> Option<&'a XrefEndpoint> {
    endpoints
        .iter()
        .filter(|ep| endpoint_matches_call(ep, call))
        .max_by_key(|ep| ep.segs.iter().filter(|s| matches!(s, Seg::Lit(_))).count())
}

fn render_route(segs: &[Seg]) -> String {
    let mut out = String::new();
    for s in segs {
        out.push('/');
        match s {
            Seg::Lit(l) => out.push_str(l),
            Seg::Wild => out.push('*'),
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn read_file<'a>(
    cache: &'a mut HashMap<String, Option<Vec<String>>>,
    root: &Path,
    file_path: &str,
) -> Option<&'a Vec<String>> {
    cache
        .entry(file_path.to_string())
        .or_insert_with(|| {
            std::fs::read_to_string(root.join(file_path))
                .ok()
                .map(|c| c.lines().map(|l| l.to_string()).collect())
        })
        .as_ref()
}

/// The entity in `ents` with the smallest span that contains `line`.
fn innermost_entity<'a>(
    ents: &[&'a super::graph::EntityInfo],
    line: usize,
) -> Option<&'a super::graph::EntityInfo> {
    ents.iter()
        .filter(|e| e.start_line <= line && line <= e.end_line)
        .min_by_key(|e| e.end_line.saturating_sub(e.start_line))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(url: &str, verb: &str) -> XrefClientCall {
        XrefClientCall {
            entity_id: "x".into(),
            name: "x".into(),
            file: "f".into(),
            line: 1,
            verb: verb.into(),
            url: url.into(),
            segs: to_segments(&format!("/api/{url}"), |s| s.contains('{')),
        }
    }
    fn endpoint(route: &str, verb: &str) -> XrefEndpoint {
        let segs = to_segments(route, |s| s.contains('{'));
        XrefEndpoint {
            entity_id: "e".into(),
            name: "e".into(),
            file: "f".into(),
            line: 1,
            verb: verb.into(),
            route: render_route(&segs),
            segs,
        }
    }

    #[test]
    fn get_hook_matches_get_endpoint_not_put() {
        // v3/parks/${parkId} is GET (usePark), PUT (updatePark), DELETE on server.
        let get_call = call("v3/parks/${parkId}", "GET");
        let get_ep = endpoint("/api/v3/parks/{id}", "GET");
        let put_ep = endpoint("/api/v3/parks/{id}", "PUT");
        assert!(endpoint_matches_call(&get_ep, &get_call));
        assert!(!endpoint_matches_call(&put_ep, &get_call)); // verb guards it
    }

    #[test]
    fn put_call_matches_put_endpoint() {
        let put_call = call("v3/parks/${parkId}", "PUT");
        let put_ep = endpoint("/api/v3/parks/{id}", "PUT");
        assert!(endpoint_matches_call(&put_ep, &put_call));
    }

    #[test]
    fn versioned_paths_do_not_cross_match() {
        let v2 = call("v2/parks/${parkId}", "GET");
        let v3_ep = endpoint("/api/v3/parks/{id}", "GET");
        assert!(!endpoint_matches_call(&v3_ep, &v2));
    }

    #[test]
    fn literal_collection_endpoint() {
        let c = call("v3/parks/ids", "POST");
        let ep = endpoint("/api/v3/parks/ids", "POST");
        assert!(endpoint_matches_call(&ep, &c));
        // and not the templated single-park route
        let single = endpoint("/api/v3/parks/{id}", "POST");
        // {id} matches the literal "ids", so this WOULD match on path; verb same.
        // best_endpoint should still prefer the more-literal /ids route.
        let eps = vec![ep, single];
        assert_eq!(best_endpoint(&eps, &c).unwrap().route, "/api/v3/parks/ids");
    }

    #[test]
    fn url_regex_extracts_verb_and_path() {
        let re = Regex::new(
            r#"(?:\b(get|post|put|patch|del)\s*(?:<[^>]*>)?\s*\(\s*)?[`"']\s*(?:/?(?:api/)?)(v\d+/[^`"'?\s]*)"#,
        )
        .unwrap();
        let src = "export const updatePark = (id) => put(`v3/parks/${id}`, request);";
        let cap = re.captures(src).unwrap();
        assert_eq!(&cap[1], "put");
        assert_eq!(&cap[2], "v3/parks/${id}");

        let swr = "useMySWR(`v3/parks/${parkId}/allBayActivitiesInPark?organisation=${orgId}`, get)";
        let cap2 = re.captures(swr).unwrap();
        assert!(cap2.get(1).is_none()); // GET default
        assert_eq!(&cap2[2], "v3/parks/${parkId}/allBayActivitiesInPark");
    }
}
