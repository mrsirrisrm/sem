//! `sem xref` — Parkable cross-repo impact across the API and its TS clients.
//!
//! Given an entity in any of the provided repos, report what it affects across
//! the client<->API boundary, joined by request URL + HTTP verb (see
//! `sem_core::parser::parkable_xref`):
//!
//!   * A JAX-RS endpoint (or an API entity an endpoint depends on) -> the client
//!     call sites that consume it, plus their in-client dependents (affected UI).
//!   * A client hook/component -> the API endpoint(s) it (transitively) calls.
//!
//! Repos are passed as `--repo tag=path` since each dev checks them out
//! somewhere different.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use colored::Colorize;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::parkable_xref::{
    best_endpoint, collect_client_calls, collect_endpoints, XrefClientCall, XrefEndpoint,
};

/// How far to walk in-client dependents when showing affected UI.
const DEFAULT_DEPTH: usize = 3;
/// Max affected entities printed per call site in terminal mode (JSON is full).
const MAX_AFFECTED_SHOWN: usize = 25;
/// How far to walk a client entity's dependencies to find its call sites.
const RESOLVE_DEPTH: usize = 8;
const BFS_CAP: usize = 20_000;

pub struct XrefOptions {
    pub entity: String,
    pub file: Option<String>,
    pub repos: Vec<(String, String)>,
    pub depth: Option<usize>,
    pub json: bool,
}

struct Repo {
    tag: String,
    graph: EntityGraph,
    endpoints: Vec<XrefEndpoint>,
    endpoint_ids: HashSet<String>,
    /// entity_id -> indices into `calls`
    calls_by_entity: HashMap<String, Vec<usize>>,
    calls: Vec<XrefClientCall>,
}

impl Repo {
    fn is_api(&self) -> bool {
        !self.endpoints.is_empty()
    }
    fn has_calls(&self) -> bool {
        !self.calls.is_empty()
    }
}

pub fn xref_command(opts: XrefOptions) {
    let depth = opts.depth.unwrap_or(DEFAULT_DEPTH);

    // --- Build every repo ----------------------------------------------------
    let mut repos: Vec<Repo> = Vec::new();
    for (tag, path) in &opts.repos {
        let root = PathBuf::from(path);
        if !root.exists() {
            eprintln!("{} repo '{}' path not found: {}", "warning:".yellow(), tag, path);
            continue;
        }
        let registry = super::create_registry(&root.to_string_lossy());
        let files = super::graph::find_supported_files_public(&root, &registry, &[]);
        let (mut graph, _entities) = EntityGraph::build(&root, &files, &registry);
        // Also resolve within-API async task edges so service->endpoint chains work.
        graph.apply_parkable_route_edges(&root);

        let endpoints = collect_endpoints(&graph, &root);
        let calls = collect_client_calls(&graph, &root);
        let endpoint_ids = endpoints.iter().map(|e| e.entity_id.clone()).collect();
        let mut calls_by_entity: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, c) in calls.iter().enumerate() {
            calls_by_entity.entry(c.entity_id.clone()).or_default().push(i);
        }
        repos.push(Repo {
            tag: tag.clone(),
            graph,
            endpoints,
            endpoint_ids,
            calls_by_entity,
            calls,
        });
    }
    if repos.is_empty() {
        eprintln!("{} no readable repos. Pass --repo tag=path.", "error:".red());
        return;
    }

    // --- Locate the target entity -------------------------------------------
    let mut matches: Vec<(usize, String)> = Vec::new();
    for (ri, repo) in repos.iter().enumerate() {
        for ent in repo.graph.entities.values() {
            if !super::entity_matches_query(ent, &opts.entity) {
                continue;
            }
            if let Some(f) = &opts.file {
                if !ent.file_path.ends_with(f.as_str()) {
                    continue;
                }
            }
            matches.push((ri, ent.id.clone()));
        }
    }
    match matches.len() {
        0 => {
            eprintln!("{} entity '{}' not found in any repo.", "error:".red(), opts.entity);
            return;
        }
        1 => {}
        _ => {
            eprintln!(
                "{} '{}' is ambiguous ({} matches). Disambiguate with --file:",
                "error:".red(),
                opts.entity,
                matches.len()
            );
            for (ri, id) in &matches {
                if let Some(e) = repos[*ri].graph.entities.get(id) {
                    eprintln!("  [{}] {} ({}:{})", repos[*ri].tag, e.name, e.file_path, e.start_line);
                }
            }
            return;
        }
    }
    let (target_ri, target_id) = matches.into_iter().next().unwrap();

    // --- Pick direction & report --------------------------------------------
    if repos[target_ri].is_api() {
        report_api_to_clients(&repos, target_ri, &target_id, depth, opts.json);
    } else {
        report_client_to_api(&repos, target_ri, &target_id, opts.json);
    }
}

/// Direction A: target is in the API repo (an endpoint, or an entity that
/// endpoints depend on). Show the client call sites that consume those
/// endpoints, plus their in-client dependents.
fn report_api_to_clients(
    repos: &[Repo],
    api_ri: usize,
    target_id: &str,
    depth: usize,
    json: bool,
) {
    let api = &repos[api_ri];

    // Which endpoints does the target represent / feed?
    let endpoint_idxs: Vec<usize> = if api.endpoint_ids.contains(target_id) {
        api.endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| e.entity_id == target_id)
            .map(|(i, _)| i)
            .collect()
    } else {
        // service/helper: endpoints that transitively depend on it
        let reach = bfs(&api.graph, target_id, Direction::Dependents, usize::MAX);
        let ids: HashSet<&str> = reach.iter().map(|(id, _)| id.as_str()).collect();
        api.endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| ids.contains(e.entity_id.as_str()))
            .map(|(i, _)| i)
            .collect()
    };

    let target_ent = api.graph.entities.get(target_id);
    let target_name = target_ent.map(|e| e.name.as_str()).unwrap_or(target_id);

    if endpoint_idxs.is_empty() {
        if json {
            println!("{}", serde_json::json!({"target": target_name, "endpoints": []}));
        } else {
            println!(
                "{} {} ({}) maps to no API endpoints.",
                "⊕".bold(),
                target_name.bold(),
                api.tag
            );
        }
        return;
    }

    let mut json_endpoints = Vec::new();
    if !json {
        println!(
            "{} {} {} ({})\n",
            "⊕".green().bold(),
            "API entity".dimmed(),
            target_name.bold(),
            api.tag.dimmed()
        );
    }

    for &ei in &endpoint_idxs {
        let ep = &api.endpoints[ei];
        if !json {
            println!(
                "  {} {} {}  {} ({}:{})",
                "▸".cyan(),
                ep.verb.cyan().bold(),
                ep.route.bold(),
                ep.name.dimmed(),
                ep.file.dimmed(),
                ep.line
            );
        }
        let mut json_consumers = Vec::new();
        for client in repos.iter().filter(|r| !r.is_api() && r.has_calls()) {
            for call in client.calls.iter().filter(|c| {
                sem_core::parser::parkable_xref::endpoint_matches_call(ep, c)
            }) {
                let affected = bfs(&client.graph, &call.entity_id, Direction::Dependents, depth);
                if json {
                    json_consumers.push(serde_json::json!({
                        "repo": client.tag,
                        "callSite": call.name,
                        "verb": call.verb,
                        "url": call.url,
                        "file": call.file,
                        "line": call.line,
                        "affected": affected.iter().filter_map(|(id, d)| {
                            client.graph.entities.get(id).map(|e| serde_json::json!({
                                "name": e.name, "file": e.file_path, "line": e.start_line, "depth": d
                            }))
                        }).collect::<Vec<_>>(),
                    }));
                } else {
                    println!(
                        "      {} [{}] {}  {} ({}:{})",
                        "←".yellow(),
                        client.tag.green(),
                        call.name.bold(),
                        format!("{} {}", call.verb, call.url).dimmed(),
                        call.file.dimmed(),
                        call.line
                    );
                    let mut shown = 0;
                    for (id, d) in &affected {
                        if let Some(e) = client.graph.entities.get(id) {
                            if shown >= MAX_AFFECTED_SHOWN {
                                break;
                            }
                            println!(
                                "        {}{} {} ({}:{})",
                                "  ".repeat(*d),
                                "←".dimmed(),
                                e.name,
                                e.file_path.dimmed(),
                                e.start_line
                            );
                            shown += 1;
                        }
                    }
                    if affected.len() > shown {
                        println!(
                            "        {}",
                            format!("… +{} more (--json for full)", affected.len() - shown).dimmed()
                        );
                    }
                }
            }
        }
        if json {
            json_endpoints.push(serde_json::json!({
                "verb": ep.verb, "route": ep.route, "handler": ep.name,
                "file": ep.file, "line": ep.line, "consumers": json_consumers,
            }));
        } else {
            println!();
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({"target": target_name, "repo": api.tag, "endpoints": json_endpoints})
        );
    }
}

/// Direction B: target is in a client repo. Find the call sites it reaches via
/// its dependencies, then the API endpoint each one hits.
fn report_client_to_api(repos: &[Repo], client_ri: usize, target_id: &str, json: bool) {
    let client = &repos[client_ri];

    // Call sites reachable from the target via its dependencies (incl. itself).
    let mut reached_ids: Vec<String> = Vec::new();
    if client.calls_by_entity.contains_key(target_id) {
        reached_ids.push(target_id.to_string());
    }
    for (id, _) in bfs(&client.graph, target_id, Direction::Dependencies, RESOLVE_DEPTH) {
        if client.calls_by_entity.contains_key(&id) && !reached_ids.contains(&id) {
            reached_ids.push(id);
        }
    }

    let target_name = client
        .graph
        .entities
        .get(target_id)
        .map(|e| e.name.as_str())
        .unwrap_or(target_id);

    // For each reached call site, find the best endpoint across API repos.
    let mut rows: Vec<(&XrefClientCall, Option<(&str, &XrefEndpoint)>)> = Vec::new();
    for cid in &reached_ids {
        for &i in &client.calls_by_entity[cid] {
            let call = &client.calls[i];
            let mut best: Option<(&str, &XrefEndpoint)> = None;
            for api in repos.iter().filter(|r| r.is_api()) {
                if let Some(ep) = best_endpoint(&api.endpoints, call) {
                    best = Some((api.tag.as_str(), ep));
                    break;
                }
            }
            rows.push((call, best));
        }
    }

    if json {
        let out = serde_json::json!({
            "target": target_name,
            "repo": client.tag,
            "calls": rows.iter().map(|(call, best)| serde_json::json!({
                "callSite": call.name, "verb": call.verb, "url": call.url,
                "file": call.file, "line": call.line,
                "endpoint": best.map(|(tag, ep)| serde_json::json!({
                    "repo": tag, "verb": ep.verb, "route": ep.route,
                    "handler": ep.name, "file": ep.file, "line": ep.line,
                })),
            })).collect::<Vec<_>>(),
        });
        println!("{out}");
        return;
    }

    println!(
        "{} {} {} ({})\n",
        "⊕".green().bold(),
        "client entity".dimmed(),
        target_name.bold(),
        client.tag.dimmed()
    );
    if rows.is_empty() {
        println!("  {} reaches no API call sites.", "✓".dimmed());
        return;
    }
    println!("  {} calls API:", "→".cyan());
    for (call, best) in &rows {
        match best {
            Some((tag, ep)) => println!(
                "    {} [{}] {} {}  {} ({}:{})",
                "→".cyan(),
                tag.green(),
                ep.verb.cyan().bold(),
                ep.route.bold(),
                ep.name.dimmed(),
                ep.file.dimmed(),
                ep.line
            ),
            None => println!(
                "    {} {} {}  {}",
                "→".yellow(),
                call.verb,
                call.url.bold(),
                "(no matching endpoint)".red().dimmed()
            ),
        }
    }
    println!();
}

#[derive(Clone, Copy)]
enum Direction {
    Dependents,
    Dependencies,
}

/// Bounded BFS over the graph, returning reached `(entity_id, depth)` pairs
/// (excluding the start). `Dependents` walks who-depends-on; `Dependencies`
/// walks what-it-uses.
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
