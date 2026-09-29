//! Audio endpoint enumeration and default-device switching.
//!
//! Two jobs, deliberately kept apart:
//!
//! * [`device`] reads what is on the machine and how Windows classifies it.
//! * [`policy`] changes which endpoint is the default, through the
//!   undocumented `IPolicyConfig` interface — the only route Windows offers.
//!
//! [`switch`] sits on top of both and holds the part that is actually worth
//! testing: which device a double-click should move to. It is pure, so the
//! cycle rule is covered exhaustively without touching hardware.
//!
//! ```no_run
//! use beautify_audio::{switch, Flow};
//!
//! let speakers: Vec<String> = vec![/* from the config */];
//! let microphones: Vec<String> = vec![];
//! match switch::switch(switch::SwitchMode::Speakers, &speakers, &microphones) {
//!     Ok(outcomes) => { /* one per flow that changed */ }
//!     Err(e) => eprintln!("{}", e.message()),
//! }
//! # let _ = Flow::Render;
//! ```

mod com;

pub mod device;
pub mod policy;
pub mod switch;

pub use device::{AudioDevice, DeviceKind, Flow};
pub use policy::available as policy_available;
pub use switch::{switch, Cycle, SwitchError, SwitchMode, SwitchOutcome, MIN_SELECTED};