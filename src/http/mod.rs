pub mod auth;
pub(crate) mod errors;
mod health;
mod routes;
pub(crate) mod validation;

pub use routes::router;
