mod cleanup;
mod registry;
mod rollback;

pub(super) use cleanup::remove_regular_handle_if_present;
#[cfg(test)]
pub(crate) use registry::recover_pending;
#[cfg(windows)]
pub(super) use registry::remove_external_lock_if_present;
pub(super) use registry::{
    recover_parent_transactions, recover_root_transactions, recover_transaction,
    try_resume_streaming_transaction,
};
pub(super) use rollback::{
    finish_committed, remove_created_output_directories, remove_created_output_directory,
    rollback_transaction,
};
