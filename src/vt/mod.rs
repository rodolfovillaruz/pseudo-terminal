//! Terminal emulation: bytes in, screen state out.

mod parser;
mod screen;

pub use parser::Parser;
#[allow(unused_imports)]
pub use parser::Perform;
pub use screen::*;
