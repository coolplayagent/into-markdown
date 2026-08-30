use super::{CliError, ExitClass, HookDecision, PreparedTransaction};

pub(in crate::transaction) fn call_hook(
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    phase: &str,
    index: usize,
    transaction: &mut PreparedTransaction,
) -> Result<(), CliError> {
    #[cfg(not(test))]
    let _ = transaction;
    match hook(phase, index)? {
        HookDecision::Continue => Ok(()),
        #[cfg(test)]
        HookDecision::SimulateCrash => {
            transaction.preserve_staged_files();
            transaction.deactivate();
            Err(CliError::new(ExitClass::Io, "simulatedCrash", format!("{phase}:{index}")))
        }
        #[cfg(test)]
        HookDecision::SimulateRollbackFailure => {
            transaction.simulate_rollback_failure = true;
            Err(CliError::new(
                ExitClass::Io,
                "injectedPermissionFailure",
                format!("deterministic rollback failure requested at {phase}:{index}"),
            ))
        }
    }
}

pub(in crate::transaction) fn crash_point(
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    phase: &str,
    index: usize,
    transaction: &mut PreparedTransaction,
) -> Result<(), CliError> {
    call_hook(hook, phase, index, transaction)
}
