pub mod traits;
pub mod factory;

pub use traits::{CliEvent, NdjsonParser};
pub use factory::create_ndjson_parser;
