use super::{
    ConversionError, Duration, ExecutionContext, Mutex, MutexGuard, PDF_CONVERSION_GATE,
    TryLockError,
};

pub(super) fn lock_pdf_conversion(
    context: &ExecutionContext,
) -> Result<MutexGuard<'static, ()>, ConversionError> {
    let gate = PDF_CONVERSION_GATE.get_or_init(|| Mutex::new(()));
    loop {
        context.checkpoint()?;
        match gate.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
            Err(TryLockError::Poisoned(_)) => {
                return Err(ConversionError::Internal {
                    detail: "PDF conversion gate is poisoned".into(),
                });
            }
        }
    }
}
