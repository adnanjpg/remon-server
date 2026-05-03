//! Alert engine
//!
//! Three submodules carry the work:
//! - `expression` — parser + AST for the rule expression DSL
//! - `resolver`   — turns an AST `MetricRef` into a label_set→value map
//!   by querying the right `metrics_*` table (lands in F3)
//! - `evaluator`  — per-rule lifecycle loop (lands in F4)

pub mod evaluator;
pub mod expression;
pub mod resolver;

pub use evaluator::spawn;
