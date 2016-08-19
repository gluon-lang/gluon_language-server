#[cfg(feature = "serde_macros")]
include!("language_server.rs.in");

#[cfg(not(feature = "serde_macros"))]
include!(concat!(env!("OUT_DIR"), "/language_server.rs"));