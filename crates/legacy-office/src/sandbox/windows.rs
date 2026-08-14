use super::Policy;

pub(super) fn install(policy: &Policy) -> Result<(), ()> {
    if !crate::windows_support::current_token_matches(&policy.app_container_sid)? {
        return Err(());
    }
    Ok(())
}
