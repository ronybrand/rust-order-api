//! The use cases and the interfaces they consume. Authorization (who may call what) is decided by the HTTP layer; the
//! rules here are the ones that hold for every caller.

use std::sync::Arc;

use time::OffsetDateTime;

pub mod customers;
pub mod orders;
pub mod ports;
pub mod search;

pub use customers::CustomerService;
pub use orders::OrderService;
pub use ports::{EventPublisher, PublishError, Session, Store};
pub use search::{Condition, Page, SearchRequest};

/// Recorded in the audit columns when no user is attached (background work).
pub const SYSTEM_ACTOR: &str = "system";

/// The authenticated caller. The audit columns (`created_by`...) always come from here: only the application knows who
/// made the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub id: String,
    pub roles: Vec<String>,
}

impl Actor {
    pub fn new(id: impl Into<String>, roles: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { id: id.into(), roles: roles.into_iter().map(Into::into).collect() }
    }

    pub fn system() -> Self {
        Self::new(SYSTEM_ACTOR, std::iter::empty::<String>())
    }

    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

/// The source of time, injectable so the tests can fix it.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(OffsetDateTime::now_utc)
}

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod actor_tests {
    use super::*;

    #[test]
    fn an_actor_knows_its_roles() {
        let actor = Actor::new("admin-1", ["user", "admin"]);
        assert!(actor.has_role("admin") && actor.has_role("user"));
        assert!(!Actor::system().has_role("admin"));
        assert_eq!(Actor::system().id, "system");
    }
}
