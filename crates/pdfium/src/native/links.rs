//! Shared bounded scan for link planning and materialization.
use super::*;

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;

enum UriReadError {
    Malformed(LinkIssueReason),
    Terminal(Error),
}

impl From<Error> for UriReadError {
    fn from(error: Error) -> Self {
        Self::Terminal(error)
    }
}

impl Native {
    fn action_uri_with_length(
        &self,
        document: usize,
        action: Handle,
        needed: c_ulong,
        maximum: u32,
    ) -> Result<String, UriReadError> {
        bounded_native_bytes("action_uri", needed, maximum, |buffer, length| unsafe {
            (self.action_uri)(document as Handle, action, buffer.cast(), length)
        })
    }

    fn web_uri_with_length(
        &self,
        web: Handle,
        index: u32,
        needed: c_int,
        maximum: u32,
    ) -> Result<String, UriReadError> {
        let index =
            c_int::try_from(index).map_err(|_| invalid("web_uri", "index exceeds C int"))?;
        if needed <= 0 {
            return Err(self.error("web_uri").into());
        }
        let units = u32::try_from(needed).map_err(|_| invalid("web_uri", "negative length"))?;
        let bytes = units.checked_mul(2).ok_or_else(|| invalid("web_uri", "length overflow"))?;
        if bytes > maximum {
            return Err(Error::ResourceLimit {
                limit: "max_link_bytes",
                actual: u64::from(bytes),
                maximum: u64::from(maximum),
            }
            .into());
        }
        let mut buffer = try_uninit_boxed_slice::<u16>(units as usize, "web_uri")?;
        // SAFETY: initialize the full fixed buffer before the native partial-write API.
        unsafe { std::ptr::write_bytes(buffer.as_mut_ptr().cast::<u16>(), 0, units as usize) };
        let copied =
            unsafe { (self.web_link_url)(web, index, buffer.as_mut_ptr().cast::<u16>(), needed) };
        if copied <= 0 || copied > needed {
            return Err(invalid("web_uri", "native length changed or was zero").into());
        }
        let copied =
            usize::try_from(copied).map_err(|_| invalid("web_uri", "length does not fit usize"))?;
        // SAFETY: the complete fixed buffer was initialized with zeroes before the FFI call.
        let mut buffer = unsafe { buffer.assume_init() }.into_vec();
        buffer.truncate(copied);
        if buffer.last() == Some(&0) {
            let _ = buffer.pop();
        }
        if buffer.contains(&0) {
            return Err(UriReadError::Malformed(LinkIssueReason::EmbeddedNul));
        }
        if char::decode_utf16(buffer.iter().copied()).any(|character| character.is_err()) {
            return Err(UriReadError::Malformed(LinkIssueReason::InvalidEncoding));
        }
        decode_utf16(&buffer).map_err(UriReadError::Terminal)
    }
}

fn bounded_native_bytes<F>(
    operation: &'static str,
    needed: c_ulong,
    maximum: u32,
    copy: F,
) -> Result<String, UriReadError>
where
    F: FnOnce(*mut u8, c_ulong) -> c_ulong,
{
    let needed_u64 = c_ulong_to_u64(needed);
    if needed == 0 {
        return Err(invalid(operation, "zero length").into());
    }
    if needed_u64 > u64::from(maximum) {
        return Err(Error::ResourceLimit {
            limit: "max_link_bytes",
            actual: needed_u64,
            maximum: u64::from(maximum),
        }
        .into());
    }
    let capacity =
        usize::try_from(needed).map_err(|_| invalid(operation, "length does not fit usize"))?;
    let mut buffer = zeroed_boxed_bytes(capacity, operation)?;
    let copied = copy(buffer.as_mut_ptr(), needed);
    if copied == 0 || copied > needed {
        return Err(invalid(operation, "native length changed or was zero").into());
    }
    let mut buffer = buffer.into_vec();
    buffer.truncate(usize::try_from(copied).unwrap_or(capacity));
    if buffer.last() == Some(&0) {
        let _ = buffer.pop();
    }
    if buffer.contains(&0) {
        return Err(UriReadError::Malformed(LinkIssueReason::EmbeddedNul));
    }
    String::from_utf8(buffer).map_err(|_| UriReadError::Malformed(LinkIssueReason::InvalidEncoding))
}

#[derive(Clone, Copy)]
enum Target {
    Annotation { action: Handle, length: c_ulong },
    Web { links: Handle, index: u32, length: c_int },
    Page(u32),
}

impl Target {
    fn allocation(self, limits: Limits) -> Result<(u64, u64), Error> {
        match self {
            Self::Annotation { length, .. } => {
                let bytes = checked_link_length(length, limits.max_link_bytes, "action_uri")?;
                Ok((bytes, bytes))
            }
            Self::Web { length, .. } => {
                let units = u64::try_from(length)
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| invalid("web_uri", "invalid URI length"))?;
                let native = units * 2;
                if native > u64::from(limits.max_link_bytes) {
                    return Err(Error::ResourceLimit {
                        limit: "max_link_bytes",
                        actual: native,
                        maximum: u64::from(limits.max_link_bytes),
                    });
                }
                Ok((units * 3, units * 5))
            }
            Self::Page(_) => Ok((0, 0)),
        }
    }
}

enum Item {
    Link { identity: LinkIdentity, bounds: PdfRect, target: Target, retained: u64, temporary: u64 },
    Omitted(LinkDiagnostic),
}

/// Link rectangles alone accept reversed finite endpoints. Clip in native
/// coordinates, then translate the clipped page origin before display rotation.
fn link_bounds(raw: [f64; 4], clip: PdfRect) -> Result<PdfRect, LinkIssueReason> {
    if raw.iter().any(|v| !v.is_finite()) {
        return Err(LinkIssueReason::NonFinite);
    }
    if raw.iter().any(|v| v.abs() > f64::from(f32::MAX)) {
        return Err(LinkIssueReason::Unrepresentable);
    }
    let [left, bottom, right, top] = raw;
    let (left, right, bottom, top) =
        (left.min(right), left.max(right), bottom.min(top), bottom.max(top));
    if left >= right || bottom >= top {
        return Err(LinkIssueReason::Empty);
    }
    let (left, bottom, right, top) = (
        left.max(f64::from(clip.left)),
        bottom.max(f64::from(clip.bottom)),
        right.min(f64::from(clip.right)),
        top.min(f64::from(clip.top)),
    );
    if left >= right || bottom >= top {
        return Err(LinkIssueReason::OutsidePage);
    }
    let local = [
        left - f64::from(clip.left),
        bottom - f64::from(clip.bottom),
        right - f64::from(clip.left),
        top - f64::from(clip.bottom),
    ];
    if local.iter().any(|value| value.abs() > f64::from(f32::MAX)) {
        return Err(LinkIssueReason::Unrepresentable);
    }
    let bounds = PdfRect {
        left: f64_to_f32(local[0]),
        bottom: f64_to_f32(local[1]),
        right: f64_to_f32(local[2]),
        top: f64_to_f32(local[3]),
    };
    if bounds.left >= bounds.right || bounds.bottom >= bounds.top {
        return Err(LinkIssueReason::Unrepresentable);
    }
    Ok(bounds)
}

struct Scan {
    plan: LinkAllocationPlan,
    fingerprint: Sha256,
}
impl Scan {
    fn consume(&mut self, count: u32, limits: Limits) -> Result<(), Error> {
        self.plan.scanned += u64::from(count);
        if self.plan.scanned > u64::from(limits.max_links_per_page) {
            return Err(Error::ResourceLimit {
                limit: "max_links_per_page",
                actual: self.plan.scanned,
                maximum: u64::from(limits.max_links_per_page),
            });
        }
        Ok(())
    }

    fn observe(
        &mut self,
        identity: LinkIdentity,
        bounds: Result<PdfRect, LinkIssueReason>,
        target: Option<Target>,
        limits: Limits,
        visit: &mut dyn FnMut(Item) -> Result<(), Error>,
    ) -> Result<(), Error> {
        match identity {
            LinkIdentity::Annotation { index } => {
                self.fingerprint.update([0]);
                self.fingerprint.update(index.to_le_bytes());
            }
            LinkIdentity::Web { index, rectangle } => {
                self.fingerprint.update([1]);
                self.fingerprint.update(index.to_le_bytes());
                self.fingerprint.update(rectangle.to_le_bytes());
            }
        }
        // Validate URI limits even for an unusable rectangle; omission never
        // bypasses a resource boundary or creates an unbudgeted allocation.
        let allocation = target.map(|t| t.allocation(limits)).transpose()?;
        match bounds {
            Err(reason) => {
                if self.plan.policy == LinkPolicy::Strict {
                    return Err(Error::Link { identity, reason });
                }
                self.fingerprint.update([2, reason as u8]);
                self.plan.diagnostics += 1;
                visit(Item::Omitted(LinkDiagnostic { identity, reason }))
            }
            Ok(bounds) => {
                self.fingerprint.update([3]);
                for value in [bounds.left, bounds.bottom, bounds.right, bounds.top] {
                    self.fingerprint.update(value.to_bits().to_le_bytes());
                }
                let Some(target) = target else {
                    self.fingerprint.update([0]);
                    return Ok(());
                };
                let (retained, temporary) = allocation.expect("target allocation was validated");
                self.fingerprint.update(retained.to_le_bytes());
                match target {
                    Target::Page(index) => {
                        self.fingerprint.update([1]);
                        self.fingerprint.update(index.to_le_bytes());
                    }
                    Target::Annotation { .. } => self.fingerprint.update([2]),
                    Target::Web { .. } => self.fingerprint.update([3]),
                }
                self.plan.count += 1;
                self.plan.target_bytes = self
                    .plan
                    .target_bytes
                    .checked_add(retained)
                    .ok_or_else(|| invalid("link_plan", "allocation overflow"))?;
                self.plan.maximum_temporary_bytes =
                    self.plan.maximum_temporary_bytes.max(temporary);
                visit(Item::Link { identity, bounds, target, retained, temporary })
            }
        }
    }

    fn finish(mut self) -> Result<LinkAllocationPlan, Error> {
        self.plan.fingerprint = self.fingerprint.finalize().into();
        self.plan.diagnostic_capacity = self
            .plan
            .diagnostics
            .checked_add(if self.plan.policy == LinkPolicy::BestEffort {
                self.plan.count
            } else {
                0
            })
            .ok_or_else(|| invalid("link_plan", "diagnostic capacity overflow"))?;
        self.plan.bytes = u64::from(self.plan.count)
            .checked_mul(std::mem::size_of::<Link>() as u64)
            .and_then(|v| {
                v.checked_add(
                    u64::from(self.plan.diagnostic_capacity)
                        * std::mem::size_of::<LinkDiagnostic>() as u64,
                )
            })
            .and_then(|v| v.checked_add(self.plan.target_bytes))
            .and_then(|v| v.checked_add(self.plan.maximum_temporary_bytes))
            .ok_or_else(|| invalid("link_plan", "allocation overflow"))?;
        Ok(self.plan)
    }
}

fn checkpoint(check: &mut dyn FnMut() -> bool) -> Result<(), Error> {
    if check() { Ok(()) } else { Err(invalid("links_checkpoint", "caller interrupted link scan")) }
}

impl Native {
    pub(super) fn plan_link_scan(
        &self,
        request: LinkRequest,
        policy: LinkPolicy,
        check: &mut dyn FnMut() -> bool,
    ) -> Result<LinkAllocationPlan, Error> {
        self.scan_links(request, policy, check, &mut |_| Ok(()))
    }
    fn scan_links(
        &self,
        request: LinkRequest,
        policy: LinkPolicy,
        check: &mut dyn FnMut() -> bool,
        visit: &mut dyn FnMut(Item) -> Result<(), Error>,
    ) -> Result<LinkAllocationPlan, Error> {
        checkpoint(check)?;
        let mut rect = FsRectF::default();
        if unsafe { (self.page_bounds)(request.page as Handle, &raw mut rect) } == 0 {
            return Err(self.error("page_bounds"));
        }
        let info = finite_rect("page_bounds", rect.left, rect.bottom, rect.right, rect.top)?;
        let mut scan = Scan {
            plan: LinkAllocationPlan { policy, ..LinkAllocationPlan::default() },
            fingerprint: Sha256::new(),
        };
        self.scan_annotations(request, info, &mut scan, check, visit)?;
        self.scan_web_links(request, info, &mut scan, check, visit)?;
        checkpoint(check)?;
        scan.finish()
    }
    fn scan_annotations(
        &self,
        request: LinkRequest,
        info: PdfRect,
        scan: &mut Scan,
        check: &mut dyn FnMut() -> bool,
        visit: &mut dyn FnMut(Item) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let LinkRequest { document, page, limits, .. } = request;
        let mut position = 0;
        loop {
            checkpoint(check)?;
            let previous = position;
            let mut link = std::ptr::null_mut();
            if unsafe { (self.enumerate_link)(page as Handle, &raw mut position, &raw mut link) }
                == 0
            {
                break;
            }
            if position <= previous || link.is_null() {
                return Err(invalid("enumerate_link", "invalid enumeration progress or handle"));
            }
            scan.consume(1, limits)?;
            let identity = LinkIdentity::Annotation {
                index: nonnegative("link_annotation_index", position - 1)?,
            };
            let mut rect = FsRectF::default();
            // GetAnnotRect reports a local boolean result; GetLastError has
            // document-load semantics and may contain an unrelated stale code.
            let bounds = if unsafe { (self.link_rect)(link, &raw mut rect) } == 0 {
                Err(LinkIssueReason::ReadFailed)
            } else {
                link_bounds(
                    [rect.left.into(), rect.bottom.into(), rect.right.into(), rect.top.into()],
                    info,
                )
            };
            let action = unsafe { (self.link_action)(link) };
            let target = if !action.is_null() && unsafe { (self.action_type)(action) } == 3 {
                Some(Target::Annotation {
                    action,
                    length: unsafe {
                        (self.action_uri)(document as Handle, action, std::ptr::null_mut(), 0)
                    },
                })
            } else {
                let destination = unsafe { (self.link_dest)(document as Handle, link) };
                if destination.is_null() {
                    None
                } else {
                    Some(Target::Page(nonnegative("dest_page_index", unsafe {
                        (self.dest_page_index)(document as Handle, destination)
                    })?))
                }
            };
            scan.observe(identity, bounds, target, limits, visit)?;
        }
        Ok(())
    }
    fn scan_web_links(
        &self,
        request: LinkRequest,
        info: PdfRect,
        scan: &mut Scan,
        check: &mut dyn FnMut() -> bool,
        visit: &mut dyn FnMut(Item) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let LinkRequest { text, limits, .. } = request;
        let web = unsafe { (self.load_web_links)(text as Handle) };
        if web.is_null() {
            return Err(self.error("load_web_links"));
        }
        let web = WebLinksGuard { raw: web, close: self.close_web_links };
        let count = nonnegative("web_link_count", unsafe { (self.web_link_count)(web.raw) })?;
        if count > limits.max_links_per_page {
            return Err(Error::ResourceLimit {
                limit: "max_links_per_page",
                actual: count.into(),
                maximum: limits.max_links_per_page.into(),
            });
        }
        for index in 0..count {
            checkpoint(check)?;
            let index_c =
                c_int::try_from(index).map_err(|_| invalid("web_links", "index exceeds C int"))?;
            let rects = nonnegative("web_link_rect_count", unsafe {
                (self.web_link_rect_count)(web.raw, index_c)
            })?;
            scan.consume(rects.max(1), limits)?;
            let target = Some(Target::Web {
                links: web.raw,
                index,
                length: unsafe { (self.web_link_url)(web.raw, index_c, std::ptr::null_mut(), 0) },
            });
            if rects == 0 {
                scan.observe(
                    LinkIdentity::Web { index, rectangle: 0 },
                    Err(LinkIssueReason::MissingRectangle),
                    target,
                    limits,
                    visit,
                )?;
            }
            for rectangle in 0..rects {
                checkpoint(check)?;
                let rectangle_c = c_int::try_from(rectangle)
                    .map_err(|_| invalid("web_link_rect", "rectangle exceeds C int"))?;
                let (mut left, mut top, mut right, mut bottom) = (0.0, 0.0, 0.0, 0.0);
                let bounds = if unsafe {
                    (self.web_link_rect)(
                        web.raw,
                        index_c,
                        rectangle_c,
                        &raw mut left,
                        &raw mut top,
                        &raw mut right,
                        &raw mut bottom,
                    )
                } == 0
                {
                    Err(LinkIssueReason::ReadFailed)
                } else {
                    link_bounds([left, bottom, right, top], info)
                };
                scan.observe(
                    LinkIdentity::Web { index, rectangle },
                    bounds,
                    target,
                    limits,
                    visit,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn extract_links(
        &self,
        request: LinkRequest,
        plan: LinkAllocationPlan,
        check: &mut dyn FnMut() -> bool,
    ) -> Result<LinkExtraction, Error> {
        let LinkRequest { document, limits, .. } = request;
        checkpoint(check)?;
        let mut links = FixedOutput::new(plan.count as usize, "links")?;
        let mut diagnostics =
            FixedOutput::new(plan.diagnostic_capacity as usize, "link_diagnostics")?;
        let mut target_bytes = 0;
        let actual = self.scan_links(request, plan.policy, check, &mut |item| match item {
            Item::Omitted(diagnostic) => diagnostics.push(diagnostic, "link_diagnostics"),
            Item::Link { identity, bounds, target, retained, temporary } => {
                if links.len() >= plan.count as usize {
                    return Err(invalid(
                        "links",
                        "materialized link count exceeded preflight plan",
                    ));
                }
                check_link_plan_bytes(target_bytes, retained, temporary, plan, "links")?;
                target_bytes += retained;
                let target = match target {
                    Target::Annotation { action, length } => self
                        .action_uri_with_length(document, action, length, limits.max_link_bytes)
                        .map(LinkTarget::ExternalUri),
                    Target::Web { links, index, length } => self
                        .web_uri_with_length(links, index, length, limits.max_link_bytes)
                        .map(LinkTarget::ExternalUri),
                    Target::Page(page_index) => Ok(LinkTarget::InternalPage { page_index }),
                };
                let target = match target {
                    Ok(target) => target,
                    Err(UriReadError::Malformed(reason))
                        if plan.policy == LinkPolicy::BestEffort =>
                    {
                        return diagnostics
                            .push(LinkDiagnostic { identity, reason }, "link_diagnostics");
                    }
                    Err(UriReadError::Malformed(reason)) => {
                        return Err(Error::Link { identity, reason });
                    }
                    Err(UriReadError::Terminal(error)) => return Err(error),
                };
                links.push(Link { identity, bounds, target }, "links")
            }
        })?;
        if actual != plan {
            return Err(invalid("links", "link scan changed after preflight"));
        }
        let links = if plan.policy == LinkPolicy::BestEffort {
            links.into_vec_prefix()
        } else {
            links.into_vec("links")?
        };
        Ok(LinkExtraction { links, diagnostics: diagnostics.into_vec_prefix() })
    }
}
