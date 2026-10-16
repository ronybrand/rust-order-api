//! Order API: customers, orders and their items, implementing the specification of the `fastapi-order-api`.

/// `Debug` for the types that hold trait objects, connections or other things with nothing useful to print: the name is
/// all a log line needs, and no field can leak into one.
macro_rules! opaque_debug {
    ($($ty:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(stringify!($ty))
            }
        }
    )+};
}

pub mod app;
pub mod domain;
pub mod error;
pub mod persistence;
