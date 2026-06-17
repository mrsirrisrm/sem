pub mod plugin;
pub mod registry;
pub mod differ;
pub mod graph;
pub mod plugins;
pub mod verify;
pub mod test_detect;
pub mod context;
#[cfg(feature = "git")]
pub mod hotspot;
mod import_resolution;
pub use import_resolution::js_ts_import_source_files_from_content;
pub mod scope_resolve;
// Parkable fork overlay: JAX-RS + Cloud Tasks async edges. Kept separate to
// minimise merge conflicts with upstream.
pub mod parkable_routes;
pub mod parkable_xref;
