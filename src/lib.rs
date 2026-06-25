#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::needless_continue)]
#![allow(clippy::single_match_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::if_not_else)]
#![allow(clippy::format_push_string)]
#![allow(clippy::expect_used)]
#![allow(clippy::unnecessary_literal_bound)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::ignored_unit_patterns)]

pub mod config;
pub mod engine;
pub mod error;
pub mod protocol;
pub mod provider;
pub mod session;
pub mod sink;
pub mod system_prompt;
pub mod tool;
