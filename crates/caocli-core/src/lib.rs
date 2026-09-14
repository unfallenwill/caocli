//! Types shared across the caocli workspace.
//!
//! The crate is intentionally tiny: it holds only the data shapes that more
//! than one workspace crate has to agree on. Larger surfaces (configuration,
//! history, the provider trait) belong in their own crates; this one exists
//! so that `caocli-core` can be a dependency edge rather than a shared lib.

mod tool;

pub use tool::{FunctionDef, ToolDef};
