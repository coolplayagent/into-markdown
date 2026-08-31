//! Request-specific worker leases bounded by the authenticated runtime policy.
use super::*;

impl ProcessPlugin {
    pub(super) fn memory_policy(
        &self,
        requested: Option<u64>,
        context: &ExecutionContext,
    ) -> Result<(RuntimePolicy, Option<ResourceReservation>), PluginError> {
        let mut policy = self.policy.clone();
        let lease = if let Some(bytes) = requested {
            if bytes == 0 || bytes > policy.max_memory_bytes {
                return Err(PluginError::new(
                    PluginErrorCode::ResourceLimit,
                    "invalid child-memory allowance",
                ));
            }
            policy.max_memory_bytes = bytes;
            Some(context.reserve_memory(bytes).map_err(|error| map_execution_error(&error))?)
        } else {
            None
        };
        Ok((policy, lease))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};

    #[test]
    fn worker_allowances_share_the_existing_credit_and_release_after_failure() {
        let process = ProcessPlugin {
            plugin: ValidatedPlugin {
                plugin_id: "fixture".into(),
                executable: PathBuf::new(),
                runtime_root: PathBuf::new(),
                executable_sha256: String::new(),
                protocol_versions: vec![1],
            },
            policy: RuntimePolicy { max_memory_bytes: 80, ..RuntimePolicy::default() },
            runtime_staging: RuntimeStaging::CopyBeforeLaunch,
        };
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 100, ..ResourceLimits::default() },
        );
        let mut envelope = context.reserve_memory(100).unwrap();
        let credited = context.with_memory_credit(&mut envelope).unwrap();
        let (policy, worker) = process.memory_policy(Some(70), &credited).unwrap();
        assert_eq!(policy.max_memory_bytes, 70);
        assert_eq!(credited.reserved_memory_bytes(), 70);
        assert_eq!(context.reserved_memory_bytes(), 100);
        assert!(process.memory_policy(Some(40), &credited).is_err());
        assert!(process.memory_policy(Some(81), &credited).is_err());
        assert!(process.memory_policy(Some(0), &credited).is_err());
        drop(worker);
        assert_eq!(credited.reserved_memory_bytes(), 0);
        drop(process.memory_policy(Some(80), &credited).unwrap());
        drop(credited);
        drop(envelope);
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.resource_usage().shared_lease_peak_bytes, 100);
    }
}
