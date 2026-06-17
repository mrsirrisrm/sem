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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Serialize;

use super::graph::{EntityGraph, EntityInfo};
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

// ---------------------------------------------------------------------------
// Report orchestration (shared by the `sem xref` CLI command and the MCP tool)
// ---------------------------------------------------------------------------

/// A built repo handed to [`build_report`]: its display tag, root, and graph.
pub struct XrefRepo<'a> {
    pub tag: String,
    pub root: &'a Path,
    pub graph: &'a EntityGraph,
}

/// Serializable cross-repo impact report.
#[derive(Serialize, Default)]
pub struct XrefReport {
    pub target: String,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub target_repo: String,
    /// `"api_to_clients"` or `"client_to_api"`.
    #[serde(skip_serializing_if = "str::is_empty")]
    pub direction: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<CallReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct EndpointReport {
    pub verb: String,
    pub route: String,
    pub handler: String,
    pub file: String,
    pub line: usize,
    pub consumers: Vec<ConsumerReport>,
}

#[derive(Serialize)]
pub struct ConsumerReport {
    pub repo: String,
    pub call_site: String,
    pub verb: String,
    pub url: String,
    pub file: String,
    pub line: usize,
    pub affected: Vec<AffectedReport>,
}

#[derive(Serialize)]
pub struct AffectedReport {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub depth: usize,
}

#[derive(Serialize)]
pub struct CallReport {
    pub call_site: String,
    pub verb: String,
    pub url: String,
    pub file: String,
    pub line: usize,
    pub endpoint: Option<EndpointRef>,
}

#[derive(Serialize)]
pub struct EndpointRef {
    pub repo: String,
    pub verb: String,
    pub route: String,
    pub handler: String,
    pub file: String,
    pub line: usize,
}

struct RepoData<'a> {
    tag: &'a str,
    graph: &'a EntityGraph,
    endpoints: Vec<XrefEndpoint>,
    endpoint_ids: HashSet<String>,
    calls: Vec<XrefClientCall>,
    calls_by_entity: HashMap<String, Vec<usize>>,
}

const BFS_CAP: usize = 20_000;
/// How far to walk a client entity's dependencies to find its call sites.
const RESOLVE_DEPTH: usize = 8;

/// Build a cross-repo impact report for `entity_query` across the given repos.
///
/// `depth` bounds the in-client dependent walk (affected UI). The result is
/// fully serializable; callers format it (terminal or JSON).
pub fn build_report(
    repos: &[XrefRepo],
    entity_query: &str,
    file: Option<&str>,
    depth: usize,
) -> XrefReport {
    let data: Vec<RepoData> = repos
        .iter()
        .map(|r| {
            let endpoints = collect_endpoints(r.graph, r.root);
            let calls = collect_client_calls(r.graph, r.root);
            let endpoint_ids = endpoints.iter().map(|e| e.entity_id.clone()).collect();
            let mut calls_by_entity: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, c) in calls.iter().enumerate() {
                calls_by_entity.entry(c.entity_id.clone()).or_default().push(i);
            }
            RepoData {
                tag: &r.tag,
                graph: r.graph,
                endpoints,
                endpoint_ids,
                calls,
                calls_by_entity,
            }
        })
        .collect();

    // Locate the target across repos.
    let mut matches: Vec<(usize, String)> = Vec::new();
    for (ri, r) in data.iter().enumerate() {
        for ent in r.graph.entities.values() {
            if !matches_query(ent, entity_query) {
                continue;
            }
            if let Some(f) = file {
                if !ent.file_path.ends_with(f) {
                    continue;
                }
            }
            matches.push((ri, ent.id.clone()));
        }
    }
    match matches.len() {
        0 => {
            return XrefReport {
                target: entity_query.to_string(),
                error: Some(format!("entity '{entity_query}' not found in any repo")),
                ..Default::default()
            }
        }
        1 => {}
        _ => {
            let list = matches
                .iter()
                .filter_map(|(ri, id)| {
                    data[*ri]
                        .graph
                        .entities
                        .get(id)
                        .map(|e| format!("[{}] {} ({}:{})", data[*ri].tag, e.name, e.file_path, e.start_line))
                })
                .collect::<Vec<_>>()
                .join("; ");
            return XrefReport {
                target: entity_query.to_string(),
                error: Some(format!(
                    "'{entity_query}' is ambiguous ({} matches) — pass file=: {list}",
                    matches.len()
                )),
                ..Default::default()
            };
        }
    }
    let (tri, tid) = matches.into_iter().next().unwrap();
    let target_name = data[tri]
        .graph
        .entities
        .get(&tid)
        .map(|e| e.name.clone())
        .unwrap_or_else(|| tid.clone());

    if !data[tri].endpoints.is_empty() {
        let endpoints = api_to_clients(&data, tri, &tid, depth);
        XrefReport {
            target: target_name,
            target_repo: data[tri].tag.to_string(),
            direction: "api_to_clients".to_string(),
            endpoints,
            ..Default::default()
        }
    } else {
        let calls = client_to_api(&data, tri, &tid);
        XrefReport {
            target: target_name,
            target_repo: data[tri].tag.to_string(),
            direction: "client_to_api".to_string(),
            calls,
            ..Default::default()
        }
    }
}

fn api_to_clients(
    data: &[RepoData],
    api_ri: usize,
    target_id: &str,
    depth: usize,
) -> Vec<EndpointReport> {
    let api = &data[api_ri];
    let endpoint_idxs: Vec<usize> = if api.endpoint_ids.contains(target_id) {
        api.endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| e.entity_id == target_id)
            .map(|(i, _)| i)
            .collect()
    } else {
        // service/helper: endpoints that transitively depend on it
        let reach = bfs(api.graph, target_id, Direction::Dependents, usize::MAX);
        let ids: HashSet<&str> = reach.iter().map(|(id, _)| id.as_str()).collect();
        api.endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| ids.contains(e.entity_id.as_str()))
            .map(|(i, _)| i)
            .collect()
    };

    let mut out = Vec::new();
    for &ei in &endpoint_idxs {
        let ep = &api.endpoints[ei];
        let mut consumers = Vec::new();
        for client in data.iter().filter(|r| r.endpoints.is_empty() && !r.calls.is_empty()) {
            for call in client.calls.iter().filter(|c| endpoint_matches_call(ep, c)) {
                let affected = bfs(client.graph, &call.entity_id, Direction::Dependents, depth)
                    .into_iter()
                    .filter_map(|(id, d)| {
                        client.graph.entities.get(&id).map(|e| AffectedReport {
                            name: e.name.clone(),
                            file: e.file_path.clone(),
                            line: e.start_line,
                            depth: d,
                        })
                    })
                    .collect();
                consumers.push(ConsumerReport {
                    repo: client.tag.to_string(),
                    call_site: call.name.clone(),
                    verb: call.verb.clone(),
                    url: call.url.clone(),
                    file: call.file.clone(),
                    line: call.line,
                    affected,
                });
            }
        }
        out.push(EndpointReport {
            verb: ep.verb.clone(),
            route: ep.route.clone(),
            handler: ep.name.clone(),
            file: ep.file.clone(),
            line: ep.line,
            consumers,
        });
    }
    out
}

fn client_to_api(data: &[RepoData], client_ri: usize, target_id: &str) -> Vec<CallReport> {
    let client = &data[client_ri];
    let mut reached_ids: Vec<String> = Vec::new();
    if client.calls_by_entity.contains_key(target_id) {
        reached_ids.push(target_id.to_string());
    }
    for (id, _) in bfs(client.graph, target_id, Direction::Dependencies, RESOLVE_DEPTH) {
        if client.calls_by_entity.contains_key(&id) && !reached_ids.contains(&id) {
            reached_ids.push(id);
        }
    }

    let mut rows = Vec::new();
    for cid in &reached_ids {
        for &i in &client.calls_by_entity[cid] {
            let call = &client.calls[i];
            let mut endpoint = None;
            for api in data.iter().filter(|r| !r.endpoints.is_empty()) {
                if let Some(ep) = best_endpoint(&api.endpoints, call) {
                    endpoint = Some(EndpointRef {
                        repo: api.tag.to_string(),
                        verb: ep.verb.clone(),
                        route: ep.route.clone(),
                        handler: ep.name.clone(),
                        file: ep.file.clone(),
                        line: ep.line,
                    });
                    break;
                }
            }
            rows.push(CallReport {
                call_site: call.name.clone(),
                verb: call.verb.clone(),
                url: call.url.clone(),
                file: call.file.clone(),
                line: call.line,
                endpoint,
            });
        }
    }
    rows
}

fn matches_query(entity: &EntityInfo, query: &str) -> bool {
    if entity.name == query {
        return true;
    }
    // "type name" form, e.g. "method retrievePark"
    if let Some((ty, name)) = query.split_once(char::is_whitespace) {
        return entity.entity_type == ty.trim() && entity.name == name.trim();
    }
    false
}

#[derive(Clone, Copy)]
enum Direction {
    Dependents,
    Dependencies,
}

fn bfs(graph: &EntityGraph, start: &str, dir: Direction, max_depth: usize) -> Vec<(String, usize)> {
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(start.to_string());
    let mut q: VecDeque<(String, usize)> = VecDeque::new();
    q.push_back((start.to_string(), 0));
    let mut out = Vec::new();
    while let Some((id, d)) = q.pop_front() {
        if d >= max_depth || seen.len() > BFS_CAP {
            continue;
        }
        let next = match dir {
            Direction::Dependents => graph.get_dependents(&id),
            Direction::Dependencies => graph.get_dependencies(&id),
        };
        for e in next {
            if seen.insert(e.id.clone()) {
                out.push((e.id.clone(), d + 1));
                q.push_back((e.id.clone(), d + 1));
            }
        }
    }
    out
}

/// Expand a leading `~`/`~/` to `$HOME`. Shells don't expand a tilde that
/// follows `=` (as in `--repo api=~/code/x`), so the value can arrive raw.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
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
