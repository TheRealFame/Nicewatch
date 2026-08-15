// Event names shared with the daemon via the Rust backend.  These mirror the
// constants in the common crate (single definition point per language).
export const EVT_HELLO = "nw/hello";
export const EVT_SNAPSHOT = "nw/snapshot";
export const EVT_DIFF = "nw/diff";
export const EVT_PROMPT = "nw/prompt";
export const EVT_WARN = "nw/warn";