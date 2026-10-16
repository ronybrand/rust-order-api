//! The notification path: the event wire format, the rules for what happens to each message (retry, dead letter,
//! deduplication), the `RabbitMQ` publisher and consumer, and the e-mail the worker sends.

pub mod delivery;
pub mod email;
pub mod event_wire;
pub mod processor;
pub mod rabbit;
pub mod smtp;

pub use delivery::{Delivered, DeliveryHandler, Mover, Outcome};
pub use email::{EmailNotifier, MailError, Mailer};
pub use processor::{Action, Message, Notifier, NotifyError, Processor, SeenSet};
pub use rabbit::{RabbitConsumer, RabbitPublisher};
pub use smtp::{SmtpMailer, SmtpSettings};
