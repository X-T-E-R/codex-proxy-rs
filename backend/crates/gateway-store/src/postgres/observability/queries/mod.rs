//! 观测查询族：dashboard / usage / ops / accounts

mod accounts;
mod dashboard;
mod ops;
mod server_errors;
mod usage;
mod usage_errors;
pub(crate) use server_errors::server_error_summary;

pub(crate) use accounts::*;
pub(crate) use dashboard::*;
pub(crate) use ops::*;
pub(crate) use usage::*;

fn literal_contains_pattern(value: &str) -> String {
    format!("%{}", literal_prefix_pattern(value))
}

pub(crate) fn literal_prefix_pattern(value: &str) -> String {
    format!(
        "{}%",
        value
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}
