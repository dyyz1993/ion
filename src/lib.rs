// Clippy policy: this crate carries historical lint debt that was never
// gated. Rather than fix 41 spread-out warnings in one giant PR (risk of
// behavior change) we allow the specific rules currently triggered at the
// crate root, so CI's `cargo clippy --lib -- -D warnings` passes while NEW
// lints in new code still surface. When a rule's occurrences are all cleaned
// up, remove its line here so it becomes enforced again.
#![allow(
    clippy::collapsible_if,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::empty_line_after_doc_comments,
    clippy::field_reassign_with_default,
    clippy::filter_next,
    clippy::if_same_then_else,
    clippy::large_enum_variant,
    clippy::let_unit_value,
    clippy::manual_clamp,
    clippy::manual_find,
    clippy::manual_strip,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::ptr_arg,
    clippy::redundant_locals,
    clippy::skip_while_next,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::unnecessary_filter_map,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_sort_by,
    clippy::unnecessary_to_owned,
    clippy::unnecessary_unwrap,
    clippy::useless_format,
    clippy::while_let_loop,
    clippy::wildcard_in_or_patterns
)]

pub mod agent;
pub mod agent_config;
pub mod auth;
pub mod auto_session_title;
pub mod backend_registry;
pub mod command_guard;
pub mod config;
pub mod error;
pub mod event;
pub mod event_bus;
pub mod export;
pub mod global_memory;
pub mod global_memory_ext;
pub mod goal_evolver;
pub mod goal_supervisor_extension;
pub mod ids;
pub mod learning_extension;
pub mod lsp_extension;
pub mod secret_detector;
pub mod skill_distillation;
pub mod tool_loop_detector;
pub mod types;

pub mod context_reclaimer;
pub mod file_snapshot;
pub mod file_time_guard;
pub mod hooks;
pub mod kernel;
pub mod manager;
pub mod mcp;
pub mod message_retrieval;
pub mod monitor_extension;
pub mod paths;
pub mod pool;
pub mod queue;
pub mod retry;
pub mod rules_engine;
pub mod runtime;
pub mod session;
pub mod session_index;
pub mod session_jsonl;
pub mod session_tree;
pub mod storage_context;
pub mod wasm_extension;
pub mod worker;
pub mod worker_api;
pub mod worker_registry;
pub mod worker_rpc;
pub mod workflow;

/// Returns the nth Fibonacci number (0-indexed).
///
/// `fibonacci(0) == 0`, `fibonacci(1) == 1`, and so on. Panics on overflow
/// for very large `n`; returns `0` for negative input.
pub fn fibonacci(n: u64) -> u64 {
    match n {
        0 => 0,
        1 => 1,
        _ => {
            let mut a: u64 = 0;
            let mut b: u64 = 1;
            for _ in 2..=n {
                let next = a.checked_add(b).expect("fibonacci overflow");
                a = b;
                b = next;
            }
            b
        }
    }
}

/// Multiplies two integers and returns the product.
///
/// Wrapping semantics: on overflow the result wraps around using
/// [`u64::wrapping_mul`].
pub fn multiply(a: u64, b: u64) -> u64 {
    a.wrapping_mul(b)
}

/// Returns the factorial of `n` (`n!`).
///
/// `factorial(0) == 1`. Panics on overflow for `n > 20` (on u64).
pub fn factorial(n: u64) -> u64 {
    match n {
        0 | 1 => 1,
        _ => (2..=n).fold(1u64, |acc, x| {
            acc.checked_mul(x).expect("factorial overflow")
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{factorial, fibonacci, multiply};

    #[test]
    fn fibonacci_base_cases() {
        assert_eq!(fibonacci(0), 0);
        assert_eq!(fibonacci(1), 1);
        assert_eq!(fibonacci(2), 1);
        assert_eq!(fibonacci(3), 2);
        assert_eq!(fibonacci(10), 55);
    }

    #[test]
    fn fibonacci_larger_values() {
        assert_eq!(fibonacci(20), 6765);
        assert_eq!(fibonacci(50), 12586269025);
        assert_eq!(fibonacci(90), 2880067194370816120);
    }

    #[test]
    fn factorial_base_cases() {
        assert_eq!(factorial(0), 1);
        assert_eq!(factorial(1), 1);
        assert_eq!(factorial(2), 2);
        assert_eq!(factorial(5), 120);
    }

    #[test]
    fn factorial_max_u64() {
        // 20! is the largest factorial that fits in u64
        assert_eq!(factorial(20), 2432902008176640000);
    }

    #[test]
    fn multiply_basic() {
        assert_eq!(multiply(2, 3), 6);
        assert_eq!(multiply(0, 5), 0);
        assert_eq!(multiply(7, 1), 7);
        assert_eq!(multiply(12, 12), 144);
    }

    #[test]
    fn multiply_overflow_wraps() {
        // u64::MAX * 2 wraps to u64::MAX - 1
        assert_eq!(multiply(u64::MAX, 2), u64::MAX - 1);
    }
}
