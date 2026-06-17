//! `sem xref` — Parkable cross-repo impact across the API and its TS clients.
//!
//! Thin orchestrator: builds an [`EntityGraph`] per `--repo tag=path`, then
//! hands them to [`sem_core::parser::parkable_xref::build_report`] (shared with
//! the MCP `sem_xref` tool) and formats the result for the terminal or `--json`.

use std::path::PathBuf;

use colored::Colorize;
use sem_core::parser::graph::EntityGraph;
use sem_core::parser::parkable_xref::{build_report, expand_tilde, XrefRepo, XrefReport};

/// How far to walk in-client dependents when showing affected UI.
const DEFAULT_DEPTH: usize = 3;
/// Max affected entities printed per call site in terminal mode (JSON is full).
const MAX_AFFECTED_SHOWN: usize = 25;

pub struct XrefOptions {
    pub entity: String,
    pub file: Option<String>,
    pub repos: Vec<(String, String)>,
    pub depth: Option<usize>,
    pub json: bool,
}

pub fn xref_command(opts: XrefOptions) {
    let depth = opts.depth.unwrap_or(DEFAULT_DEPTH);

    // Build each repo's graph (fresh; xref is not on the cache path).
    let mut built: Vec<(String, PathBuf, EntityGraph)> = Vec::new();
    for (tag, path) in &opts.repos {
        let root = expand_tilde(path);
        if !root.exists() {
            eprintln!("{} repo '{}' path not found: {}", "warning:".yellow(), tag, path);
            continue;
        }
        let registry = super::create_registry(&root.to_string_lossy());
        let files = super::graph::find_supported_files_public(&root, &registry, &[]);
        let (mut graph, _entities) = EntityGraph::build(&root, &files, &registry);
        graph.apply_parkable_route_edges(&root);
        built.push((tag.clone(), root, graph));
    }
    if built.is_empty() {
        eprintln!("{} no readable repos. Pass --repo tag=path.", "error:".red());
        return;
    }

    let repos: Vec<XrefRepo> = built
        .iter()
        .map(|(tag, root, graph)| XrefRepo {
            tag: tag.clone(),
            root: root.as_path(),
            graph,
        })
        .collect();

    let report = build_report(&repos, &opts.entity, opts.file.as_deref(), depth);

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        print_report(&report);
    }
}

fn print_report(report: &XrefReport) {
    if let Some(err) = &report.error {
        eprintln!("{} {}", "error:".red().bold(), err);
        return;
    }

    match report.direction.as_str() {
        "api_to_clients" => {
            println!(
                "{} {} {} ({})\n",
                "⊕".green().bold(),
                "API entity".dimmed(),
                report.target.bold(),
                report.target_repo.dimmed()
            );
            if report.endpoints.is_empty() {
                println!("  maps to no API endpoints.");
            }
            for ep in &report.endpoints {
                println!(
                    "  {} {} {}  {} ({}:{})",
                    "▸".cyan(),
                    ep.verb.cyan().bold(),
                    ep.route.bold(),
                    ep.handler.dimmed(),
                    ep.file.dimmed(),
                    ep.line
                );
                if ep.consumers.is_empty() {
                    println!("      {}", "(no client consumers found)".dimmed());
                }
                for c in &ep.consumers {
                    println!(
                        "      {} [{}] {}  {} ({}:{})",
                        "←".yellow(),
                        c.repo.green(),
                        c.call_site.bold(),
                        format!("{} {}", c.verb, c.url).dimmed(),
                        c.file.dimmed(),
                        c.line
                    );
                    for a in c.affected.iter().take(MAX_AFFECTED_SHOWN) {
                        println!(
                            "        {}{} {} ({}:{})",
                            "  ".repeat(a.depth),
                            "←".dimmed(),
                            a.name,
                            a.file.dimmed(),
                            a.line
                        );
                    }
                    if c.affected.len() > MAX_AFFECTED_SHOWN {
                        println!(
                            "        {}",
                            format!(
                                "… +{} more (--json for full)",
                                c.affected.len() - MAX_AFFECTED_SHOWN
                            )
                            .dimmed()
                        );
                    }
                }
                println!();
            }
        }
        "client_to_api" => {
            println!(
                "{} {} {} ({})\n",
                "⊕".green().bold(),
                "client entity".dimmed(),
                report.target.bold(),
                report.target_repo.dimmed()
            );
            if report.calls.is_empty() {
                println!("  {} reaches no API call sites.", "✓".dimmed());
                return;
            }
            println!("  {} calls API:", "→".cyan());
            for call in &report.calls {
                match &call.endpoint {
                    Some(ep) => println!(
                        "    {} [{}] {} {}  {} ({}:{})",
                        "→".cyan(),
                        ep.repo.green(),
                        ep.verb.cyan().bold(),
                        ep.route.bold(),
                        ep.handler.dimmed(),
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
        _ => {}
    }
}
