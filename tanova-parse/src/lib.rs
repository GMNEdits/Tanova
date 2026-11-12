//! Tanova `.nk` parser.
//!
//! Implements a TCL stack machine (push/pop) that turns `.nk` syntax into
//! kernel commands and node state, plus the small math-expression evaluator
//! Nuke scripts use inside knobs.

pub mod expr_tcl;
pub mod nk_stack;

pub use expr_tcl::{TclEvalEnv, eval_tcl_expr, expand_tcl_string};
pub use nk_stack::{ParsedNode, ParsedScript, load_nk_into_dag, parse_nk_script};

/// Everything that can go wrong while reading a `.nk` script or expression.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The script was not valid TCL/Nuke stack syntax.
    #[error("line {line}: {message}")]
    Syntax {
        /// 1-based line number.
        line: usize,
        /// What failed.
        message: String,
    },

    /// A TCL math expression could not be evaluated.
    #[error("expression `{expr}`: {message}")]
    Expr {
        /// The original expression text.
        expr: String,
        /// What failed.
        message: String,
    },

    /// The kernel rejected a node, edge, or knob.
    #[error("kernel: {0}")]
    Kernel(String),
}
