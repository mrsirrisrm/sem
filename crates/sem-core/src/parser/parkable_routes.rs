//! Parkable-specific async route edges (JAX-RS + GCP Cloud Tasks).
//!
//! This is a **Parkable fork overlay**, deliberately kept out of the upstream
//! extraction/resolution pipeline so it survives `git merge upstream/main` with
//! minimal conflict surface. It runs *after* a graph is built, reads only the
//! public `EntityGraph` surface (`file_path` + line ranges) plus the raw source,
//! and appends synthetic edges. Nothing in here is general-purpose; it is keyed
//! to Parkable conventions:
//!
//!   * Producers enqueue Cloud Tasks via `TaskOptions.builder()....withUrl(<url>)`.
//!   * Consumers are JAX-RS resources: a class-level `@Path` base joined with a
//!     method-level `@Path` and an HTTP verb annotation (`@POST`, `@GET`, ...).
//!
//! The async hop (producer `.withUrl("...")` -> GCP -> JAX-RS handler) is keyed
//! only by a URL string, so normal identifier-based resolution can't see it.
//! We close that gap by matching producer URLs against handler routes and
//! emitting a `Calls` edge producer-method -> handler-method, which then flows
//! through `sem impact` for free.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use regex::Regex;

use super::graph::{EntityGraph, EntityRef, RefType};

/// A single path segment, after normalising both JAX-RS templates (`{id}`) and
/// producer-side concatenation gaps (`"/foo/" + id`) to the same `Wild` form.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Lit(String),
    Wild,
}

/// Sentinel char standing in for a dynamic (non-string-literal) run inside a
/// `.withUrl(...)` argument. Not `/` (so it stays within one segment) and not a
/// real path char.
const DYN: char = '\u{1}';

/// Summary returned to the caller for optional logging. The CLI hook can ignore
/// it; surfacing it makes silent misses (unresolvable dynamic URLs) visible.
#[derive(Debug, Default, Clone)]
pub struct RouteEdgeStats {
    pub handlers: usize,
    pub edges_added: usize,
    /// `.withUrl(...)` sites whose URL could not be resolved to a static-enough
    /// pattern (e.g. `.withUrl(url)` with a bare variable).
    pub producers_unresolved: usize,
}

impl EntityGraph {
    /// Parkable overlay: link Cloud-Task producers to their JAX-RS handlers.
    ///
    /// Idempotent-ish: it dedupes against existing `(from, to)` edges, so running
    /// it twice will not double up. `root` is the repo root; entity `file_path`s
    /// are resolved relative to it.
    pub fn apply_parkable_route_edges(&mut self, root: &Path) -> RouteEdgeStats {
        let path_re = Regex::new(r#"@Path\s*\(\s*(?:value\s*=\s*)?"([^"]*)""#).unwrap();
        let verb_re = Regex::new(r"@(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)\b").unwrap();

        let mut file_cache: HashMap<String, Option<Vec<String>>> = HashMap::new();

        // --- Pass 1: base `@Path` per container (class/interface) -------------
        // Keyed by container entity id -> base path segments.
        let mut base_by_id: HashMap<String, Vec<Seg>> = HashMap::new();
        for ent in self.entities.values() {
            if !is_container(&ent.entity_type) {
                continue;
            }
            let Some(header) = entity_header(&mut file_cache, root, ent, &container_keywords()) else {
                continue;
            };
            if let Some(cap) = path_re.captures(&header) {
                base_by_id.insert(ent.id.clone(), jaxrs_segments(&cap[1]));
            }
        }

        // --- Pass 2: handler routes (method + verb) --------------------------
        struct Handler {
            id: String,
            segs: Vec<Seg>,
            lit_count: usize,
        }
        let mut handlers: Vec<Handler> = Vec::new();
        for ent in self.entities.values() {
            if ent.entity_type != "method" {
                continue;
            }
            let Some(header) = entity_header(&mut file_cache, root, ent, &[ent.name.clone()]) else {
                continue;
            };
            // A JAX-RS endpoint method carries an HTTP verb annotation.
            if !verb_re.is_match(&header) {
                continue;
            }
            let mut segs = ent
                .parent_id
                .as_ref()
                .and_then(|pid| base_by_id.get(pid))
                .cloned()
                .unwrap_or_default();
            if let Some(cap) = path_re.captures(&header) {
                segs.extend(jaxrs_segments(&cap[1]));
            }
            if segs.is_empty() {
                continue;
            }
            let lit_count = segs.iter().filter(|s| matches!(s, Seg::Lit(_))).count();
            handlers.push(Handler {
                id: ent.id.clone(),
                segs,
                lit_count,
            });
        }

        // --- Pass 3: producers (`.withUrl(...)`) -> best matching handler -----
        let mut new_edges: Vec<(String, String)> = Vec::new();
        let mut unresolved = 0usize;
        for ent in self.entities.values() {
            if ent.entity_type != "method" {
                continue;
            }
            let Some(body) = entity_slice(&mut file_cache, root, ent) else {
                continue;
            };
            for arg in with_url_args(&body) {
                match producer_segments(&arg) {
                    Some(prod) => {
                        // Most specific (most literal segments) handler wins.
                        if let Some(h) = handlers
                            .iter()
                            .filter(|h| routes_match(&h.segs, &prod))
                            .max_by_key(|h| h.lit_count)
                        {
                            if h.id != ent.id {
                                new_edges.push((ent.id.clone(), h.id.clone()));
                            }
                        }
                    }
                    None => unresolved += 1,
                }
            }
        }

        // --- Commit edges (dedupe against existing + self) -------------------
        let mut seen: HashSet<(String, String)> = self
            .edges
            .iter()
            .map(|e| (e.from_entity.clone(), e.to_entity.clone()))
            .collect();
        let mut added = 0usize;
        for (from, to) in new_edges {
            if !seen.insert((from.clone(), to.clone())) {
                continue;
            }
            self.dependents
                .entry(to.clone())
                .or_default()
                .push(from.clone());
            self.dependencies
                .entry(from.clone())
                .or_default()
                .push(to.clone());
            self.edges.push(EntityRef {
                from_entity: from,
                to_entity: to,
                ref_type: RefType::Calls,
            });
            added += 1;
        }

        RouteEdgeStats {
            handlers: handlers.len(),
            edges_added: added,
            producers_unresolved: unresolved,
        }
    }
}

fn is_container(entity_type: &str) -> bool {
    matches!(entity_type, "class" | "interface" | "enum" | "record")
}

fn container_keywords() -> Vec<String> {
    ["class", "interface", "enum", "record"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Read the file backing `file_path` (relative to `root`) into lines, cached.
fn load_lines<'a>(
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

/// Full source slice for an entity (`start_line..=end_line`, 1-based).
fn entity_slice(
    cache: &mut HashMap<String, Option<Vec<String>>>,
    root: &Path,
    ent: &super::graph::EntityInfo,
) -> Option<String> {
    let lines = load_lines(cache, root, &ent.file_path)?;
    let start = ent.start_line.saturating_sub(1);
    let end = ent.end_line.min(lines.len());
    if start >= end {
        return None;
    }
    Some(lines[start..end].join("\n"))
}

/// The annotation/header portion of an entity: lines from its start up to (but
/// not including) the first line declaring the entity (the line containing one
/// of `markers` as a word). For Java the span already includes leading
/// annotations, so this isolates them from the body.
fn entity_header(
    cache: &mut HashMap<String, Option<Vec<String>>>,
    root: &Path,
    ent: &super::graph::EntityInfo,
    markers: &[String],
) -> Option<String> {
    let lines = load_lines(cache, root, &ent.file_path)?;
    let start = ent.start_line.saturating_sub(1);
    let end = ent.end_line.min(lines.len());
    if start >= end {
        return None;
    }
    let slice = &lines[start..end];
    let decl = slice
        .iter()
        .position(|l| markers.iter().any(|m| line_declares(l, m)))
        .map(|p| p + 1) // include the declaration line itself (annotation can share it)
        .unwrap_or(slice.len());
    Some(slice[..decl].join("\n"))
}

/// True if `line` contains `marker` as a standalone token (word boundary),
/// e.g. `class Foo` or `void asyncCommandCheck(`.
fn line_declares(line: &str, marker: &str) -> bool {
    let Some(pos) = line.find(marker) else {
        return false;
    };
    let before_ok = line[..pos]
        .chars()
        .next_back()
        .map_or(true, |c| !c.is_alphanumeric() && c != '_');
    let after_ok = line[pos + marker.len()..]
        .chars()
        .next()
        .map_or(true, |c| !c.is_alphanumeric() && c != '_');
    before_ok && after_ok
}

/// Split a JAX-RS path template into segments; `{x}` becomes `Wild`.
fn jaxrs_segments(path: &str) -> Vec<Seg> {
    to_segments(path, |s| s.contains('{'))
}

fn to_segments(path: &str, is_wild: impl Fn(&str) -> bool) -> Vec<Seg> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if is_wild(s) || s.contains(DYN) {
                Seg::Wild
            } else {
                Seg::Lit(s.to_string())
            }
        })
        .collect()
}

/// Extract every `.withUrl( <arg> )` argument expression (paren-balanced,
/// string-aware) from a chunk of source.
fn with_url_args(src: &str) -> Vec<String> {
    let mut args = Vec::new();
    let bytes = src.as_bytes();
    let needle = ".withUrl(";
    let mut search = 0;
    while let Some(rel) = src[search..].find(needle) {
        let open = search + rel + needle.len();
        let mut depth = 1i32;
        let mut i = open;
        let mut in_str = false;
        while i < src.len() {
            let c = bytes[i];
            if in_str {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        if depth == 0 && i <= src.len() {
            args.push(src[open..i].to_string());
        }
        search = open;
    }
    args
}

/// Turn a `.withUrl(...)` argument into path segments, collapsing dynamic runs
/// (`+ expr +`) to `Wild`. Returns `None` if there is no leading string literal
/// to anchor the path (e.g. a bare `url` variable), which we can't resolve.
fn producer_segments(arg: &str) -> Option<Vec<Seg>> {
    let bytes = arg.as_bytes();
    let mut canonical = String::new();
    let mut had_literal = false;
    let mut last_dynamic = false;
    let mut i = 0;
    while i < arg.len() {
        let c = bytes[i];
        if c == b'"' {
            let mut j = i + 1;
            while j < arg.len() && bytes[j] != b'"' {
                if bytes[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            canonical.push_str(&arg[i + 1..j.min(arg.len())]);
            had_literal = true;
            last_dynamic = false;
            i = j + 1;
        } else if c.is_ascii_whitespace() || c == b'+' {
            i += 1;
        } else {
            if !last_dynamic {
                canonical.push(DYN);
                last_dynamic = true;
            }
            i += 1;
        }
    }
    if !had_literal {
        return None;
    }
    // Drop query string; require an absolute, literal-anchored path.
    let canonical = canonical.split('?').next().unwrap_or("").to_string();
    if !canonical.starts_with('/') {
        return None;
    }
    Some(to_segments(&canonical, |_| false))
}

/// Do a handler route and a producer URL describe the same path?
///
/// Matching is **asymmetric**: the handler is the pattern. A handler `{id}`
/// template segment matches anything, but a handler *literal* segment must be
/// supplied literally by the producer — a producer's dynamic segment does NOT
/// satisfy a handler literal. This prevents two distinct routes whose wildcards
/// sit in different positions (e.g. `/x/{id}/apply` vs `/x/billing/{id}`) from
/// cross-matching. Lengths must be equal.
fn routes_match(handler: &[Seg], producer: &[Seg]) -> bool {
    handler.len() == producer.len()
        && handler.iter().zip(producer).all(|(h, p)| match h {
            Seg::Wild => true,
            Seg::Lit(a) => matches!(p, Seg::Lit(b) if a == b),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Seg {
        Seg::Lit(s.to_string())
    }

    #[test]
    fn jaxrs_template_to_segments() {
        assert_eq!(
            jaxrs_segments("/api/v1/tasks/accessGateTasks"),
            vec![lit("api"), lit("v1"), lit("tasks"), lit("accessGateTasks")]
        );
        assert_eq!(
            jaxrs_segments("/billingParkingSubscriptions/{id}"),
            vec![lit("billingParkingSubscriptions"), Seg::Wild]
        );
    }

    #[test]
    fn producer_pure_literal() {
        assert_eq!(
            producer_segments(r#""/api/v1/tasks/half-hourly-job""#),
            Some(vec![lit("api"), lit("v1"), lit("tasks"), lit("half-hourly-job")])
        );
    }

    #[test]
    fn producer_trailing_concat_id() {
        // "/api/v2/subscriptiontasks/billingParkingSubscriptions/" + territory.getId()
        let seg = producer_segments(
            r#""/api/v2/subscriptiontasks/billingParkingSubscriptions/" + territory.getId()"#,
        )
        .unwrap();
        assert_eq!(seg.last(), Some(&Seg::Wild));
        assert_eq!(seg.len(), 5);
    }

    #[test]
    fn producer_strips_query_and_rejects_bare_var() {
        assert_eq!(
            producer_segments(r#""/api/v1/test?query=true""#),
            Some(vec![lit("api"), lit("v1"), lit("test")])
        );
        assert_eq!(producer_segments("url"), None);
    }

    #[test]
    fn templated_handler_matches_concatenated_producer() {
        let handler = jaxrs_segments("/api/v2/subscriptiontasks/billingParkingSubscriptions/{id}");
        let producer = producer_segments(
            r#""/api/v2/subscriptiontasks/billingParkingSubscriptions/" + territory.getId()"#,
        )
        .unwrap();
        assert!(routes_match(&handler, &producer));
    }

    #[test]
    fn canonical_handler_matches_canonical_producer() {
        let handler = {
            let mut s = jaxrs_segments("/api/v1/tasks/accessGateTasks");
            s.extend(jaxrs_segments("/teltonikaCommandCheck"));
            s
        };
        let producer =
            producer_segments(r#""/api/v1/tasks/accessGateTasks/teltonikaCommandCheck""#).unwrap();
        assert!(routes_match(&handler, &producer));
    }

    #[test]
    fn non_matching_routes_rejected() {
        let handler = jaxrs_segments("/api/v1/tasks/foo");
        let producer = producer_segments(r#""/api/v1/tasks/bar""#).unwrap();
        assert!(!routes_match(&handler, &producer));
    }

    #[test]
    fn crossed_wildcards_do_not_false_match() {
        // Real case: producer "/sub/" + id + "/apply" must NOT match handler
        // "/sub/billing/{id}" just because each side has one wildcard.
        let handler = {
            let mut s = jaxrs_segments("/api/v2/subscriptiontasks");
            s.extend(jaxrs_segments("/billingParkingSubscriptions/{territoryId}"));
            s
        };
        let producer = producer_segments(
            r#""/api/v2/subscriptiontasks/" + territoryId + "/apply-scheduled-price-changes""#,
        )
        .unwrap();
        assert!(!routes_match(&handler, &producer));

        // ...but it DOES match its own templated handler.
        let correct = {
            let mut s = jaxrs_segments("/api/v2/subscriptiontasks");
            s.extend(jaxrs_segments("/{territoryId}/apply-scheduled-price-changes"));
            s
        };
        assert!(routes_match(&correct, &producer));
    }

    #[test]
    fn extracts_multiple_with_url_args() {
        let src = r#"
            .withUrl("/api/a")
            .withUrl("/api/b/" + thing.id())
        "#;
        let args = with_url_args(src);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], r#""/api/a""#);
    }
}
