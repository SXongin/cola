//! The backend contract: the neutral read model the Bridge consumes (ADR-0053).
//!
//! The seam's read side carries a [`SessionTranscript`] — typed messages and
//! typed parts — instead of backend wire JSON. Backend protocol field names
//! live only in the adapter's private decoders, so a protocol change is an
//! adapter change, not a Bridge-and-card change.
//!
//! The `Backend`/`DirectoryBackend` traits themselves move here in the
//! migration's final step (#338); this module starts as the read model so the
//! adapter can produce it while the existing wire-typed read keeps working
//! (spec #332, the expand step).

pub mod transcript;

// The neutral views are re-exported at the contract root: consumers import
// them from here, never from the decoder's module path.
pub use transcript::*;
