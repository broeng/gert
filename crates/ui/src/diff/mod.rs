//! A standalone diff widget: layout model ([`layout`], pure) and a
//! virtualized GPUI view ([`DiffView`]) on top of it. The view is a
//! variable-height `list`, so rows other than code lines can sit between lines.
//!
//! The owner creates a [`DiffView`] with a [`model::Diff`] and resolved
//! preferences and subscribes to [`DiffEvent`]. Unchanged lines are folded
//! and unfolded inside the widget; the owner is never asked for more lines.
//! Key bindings live in [`actions`] (key context
//! `DiffView`); call [`bind_keys`] once (`ui::init` does).

pub mod actions;
pub mod layout;
mod paint;
mod view;

#[cfg(test)]
pub(crate) mod tests;

pub use actions::{DIFF_VIEW, bind_keys};
pub use layout::{Anchor, Expansion, FoldRow, Layout, LayoutOptions};
pub use view::{DiffEvent, DiffSizing, DiffStatus, DiffView};
