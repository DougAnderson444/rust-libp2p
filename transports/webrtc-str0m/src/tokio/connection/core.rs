//! The core connection fields as a separate module, so that we can easily clone it and move into a
//! task that is spawned on the tokio runtime.
//!
//! The Core Connection components are typially Arc<Mutex<>> wrapped, so that they can be shared.
//!
//! Any methods that need to be spawned(like .run()) should be called on the cloned core connection.
//!
//! The top level Connection cannot be cloned as it holds the DropListener which cannot be bound by
//! [std::clone::Clone].
