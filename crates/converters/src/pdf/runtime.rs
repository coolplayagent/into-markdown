use super::{ConversionError, ExecutionContext};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Poll, Waker};
use std::time::Duration;

static PDF_CONVERSION_ACTIVE: AtomicBool = AtomicBool::new(false);
static WAITERS: Mutex<Vec<Waker>> = Mutex::new(Vec::new());

// PDFium admits only one live runtime. This permit owns that lifetime, not a
// thread-bound mutex guard: OCR may await while the document stays open.
pub(super) struct PdfConversionPermit;

impl Drop for PdfConversionPermit {
    fn drop(&mut self) {
        let mut waiters = WAITERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        PDF_CONVERSION_ACTIVE.store(false, Ordering::Release);
        let ready = std::mem::take(&mut *waiters);
        drop(waiters);
        for waker in ready {
            waker.wake();
        }
    }
}

pub(super) async fn acquire_pdf_conversion(
    context: &ExecutionContext,
) -> Result<PdfConversionPermit, ConversionError> {
    std::future::poll_fn(|task| {
        context.checkpoint()?;
        // Serialize registration with release so no wakeup can be lost.
        let mut waiters = WAITERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if PDF_CONVERSION_ACTIVE
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Poll::Ready(Ok(PdfConversionPermit))
        } else {
            if !waiters.iter().any(|waker| waker.will_wake(task.waker())) {
                waiters.push(task.waker().clone());
            }
            Poll::Pending
        }
    })
    .await
}

pub(super) fn lock_pdf_conversion(
    context: &ExecutionContext,
) -> Result<PdfConversionPermit, ConversionError> {
    loop {
        context.checkpoint()?;
        if PDF_CONVERSION_ACTIVE
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(PdfConversionPermit);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
