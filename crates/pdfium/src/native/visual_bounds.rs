//! Bounded page-object inventory for layout and composite visual recovery.
use super::*;

impl Native {
    pub(super) fn plan_visual_bounds(
        &self,
        page: usize,
        max_objects: u32,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<PathBoundsAllocationPlan, Error> {
        let count = nonnegative("object_count", unsafe { (self.object_count)(page as Handle) })?;
        if count > max_objects {
            return Err(Error::ResourceLimit {
                limit: "max_page_objects",
                actual: u64::from(count),
                maximum: u64::from(max_objects),
            });
        }
        let mut paths = 0_u32;
        let mut contains_composite_visuals = false;
        for index in 0..count {
            path_scan_checkpoint(index, "path_bounds_plan_checkpoint", checkpoint)?;
            let object = unsafe {
                (self.get_object)(
                    page as Handle,
                    c_int::try_from(index)
                        .map_err(|_| invalid("path_bounds_plan", "index exceeds C int"))?,
                )
            };
            if object.is_null() {
                return Err(self.error("get_object"));
            }
            let kind = unsafe { (self.object_type)(object) };
            contains_composite_visuals |= matches!(kind, 4 | 5);
            if kind == 2 {
                // Validate the same bounds during preflight so allocation is
                // never authorized by a malformed or non-finite PATH object.
                object_bounds(self, object, "path_bounds_plan")?;
                paths = paths
                    .checked_add(1)
                    .ok_or_else(|| invalid("path_bounds_plan", "count overflow"))?;
            }
        }
        let bytes = u64::from(paths)
            .checked_mul(u64::try_from(std::mem::size_of::<PdfRect>()).unwrap_or(u64::MAX))
            .ok_or_else(|| invalid("path_bounds_plan", "allocation overflow"))?;
        Ok(PathBoundsAllocationPlan { bytes, count: paths, contains_composite_visuals })
    }
}
