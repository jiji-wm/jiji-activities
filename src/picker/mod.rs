//! Picker adapters.
//!
//! Two sibling submodules:
//!
//! - [`single_select`] — `fuzzel --dmenu`-backed single-select picker
//!   used by `switch`, `move-window`, `move-workspace`, and the
//!   chained-single leg of `assign-workspace`.
//! - [`multi_select`] — `rofi -dmenu -multi-select`-backed multi-select
//!   picker used by `assign-workspace`. Surfaces two sentinel rows
//!   (`« Select all »`, `« Only one… »`) that resolve to control-flow
//!   outcomes rather than literal selections.
//!
//! The two pickers use different external binaries because no single
//! widely-deployed Wayland picker handles both ergonomic single-select
//! (`fuzzel --dmenu` is the de-facto standard) and multi-select
//! (`rofi --multi-select` is the de-facto standard) well. We accept
//! the two-dep cost in exchange for the better UX on each path.
//!
//! Back-compat re-exports preserve the pre-reshape call sites
//! (`crate::picker::pick_one`, `ensure_available`, `PickerOutcome`,
//! `PICKER_MISSING_MESSAGE`) so the move was a no-op for callers
//! outside this module.

pub(crate) mod multi_select;
mod single_select;

// `PICKER_MISSING_MESSAGE` is re-exported even though no current caller
// outside `single_select` references it via the `crate::picker::` path:
// it preserves the pre-reshape backwards-compatibility surface so a
// future test or doc reference doesn't have to know about the
// single_select submodule split.
#[allow(unused_imports)]
pub(crate) use single_select::{PICKER_MISSING_MESSAGE, PickerOutcome, ensure_available, pick_one};
