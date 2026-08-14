//! External clients: opencode (C6), cmux (C5), SSE observer (C7), and the
//! manager (C11). Everything that touches a socket or spawns a process lives
//! here.

pub mod cmux;
pub mod driver;
pub mod driver_cmux;
pub mod manager;
pub mod opencode;
pub mod registry;
pub mod sse;
