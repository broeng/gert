//! A standalone diff widget: layout model ([`layout`], pure) and a
//! virtualized GPUI view ([`DiffView`]) on top of it.
//!
//! The owner creates a [`DiffView`] with a [`model::Diff`] and resolved
//! preferences, subscribes to [`DiffEvent`], and hands in a new diff when
//! lines were expanded. Key bindings live in [`actions`] (key context
//! `DiffView`); call [`bind_keys`] once (`ui::init` does).

pub mod actions;
pub mod layout;
mod paint;
mod view;

#[cfg(test)]
pub(crate) mod tests;

pub use actions::{DIFF_VIEW, bind_keys};
pub use layout::{Anchor, Layout, LayoutOptions, SkipRow};
pub use view::{DiffEvent, DiffSizing, DiffStatus, DiffView, Expand};
