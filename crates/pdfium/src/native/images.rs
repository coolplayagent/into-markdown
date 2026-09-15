use super::*;

impl Native {
    // The raw image API discards PDF soft masks. Render at source resolution so
    // OCR and published assets receive the visible image, including its alpha.
    pub(super) fn render_image_bitmap(
        &self,
        document: usize,
        page: usize,
        image: usize,
    ) -> Result<BitmapGuard, Error> {
        let image = image as Handle;
        let mut original = text::Matrix::default();
        if unsafe { (self.object_matrix)(image, &raw mut original) } == 0 {
            return Err(self.error("image_matrix"));
        }
        let (mut width, mut height) = (0_u32, 0_u32);
        if unsafe { (self.image_pixel_size)(image, &raw mut width, &raw mut height) } == 0 {
            return Err(self.error("image_pixel_size"));
        }
        let pixels = text::Matrix::image_pixels(width, height);
        if unsafe { (self.set_object_matrix)(image, &raw const pixels) } == 0 {
            return Err(self.error("image_matrix_normalize"));
        }
        // The runtime gate serializes this temporary change. Restore before
        // inspecting the result, including a native render failure.
        let raw = unsafe { (self.image_bitmap)(document as Handle, page as Handle, image) };
        let restored = unsafe { (self.set_object_matrix)(image, &raw const original) } != 0;
        if raw.is_null() {
            return Err(self.error("image_bitmap"));
        }
        let bitmap = BitmapGuard { raw, destroy: self.destroy_bitmap };
        if !restored {
            return Err(self.error("image_matrix_restore"));
        }
        Ok(bitmap)
    }
    pub(super) fn copy_image_bitmap(
        &self,
        bitmap: BitmapGuard,
        limits: Limits,
        planned_bytes: u64,
    ) -> Result<ImageBitmap, Error> {
        let width = nonnegative("image_bitmap_width", unsafe { (self.bitmap_width)(bitmap.raw) })?;
        let height =
            nonnegative("image_bitmap_height", unsafe { (self.bitmap_height)(bitmap.raw) })?;
        let stride =
            nonnegative("image_bitmap_stride", unsafe { (self.bitmap_stride)(bitmap.raw) })?;
        for dimension in [width, height] {
            if dimension > limits.max_render_dimension {
                return Err(Error::ResourceLimit {
                    limit: "max_render_dimension",
                    actual: u64::from(dimension),
                    maximum: u64::from(limits.max_render_dimension),
                });
            }
        }
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| invalid("image_bitmap", "pixel count overflow"))?;
        if pixels > limits.max_render_pixels {
            return Err(Error::ResourceLimit {
                limit: "max_render_pixels",
                actual: pixels,
                maximum: limits.max_render_pixels,
            });
        }
        let (format, bytes_per_pixel) = match unsafe { (self.bitmap_format)(bitmap.raw) } {
            1 => (PixelFormat::Gray, 1_u32),
            2 => (PixelFormat::Bgr, 3),
            3 => (PixelFormat::Bgrx, 4),
            4 => (PixelFormat::Bgra, 4),
            value => return Err(invalid("image_bitmap_format", &format!("unknown {value}"))),
        };
        let minimum_stride = width
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid("image_bitmap", "minimum stride overflow"))?;
        if stride < minimum_stride {
            return Err(invalid("image_bitmap", "native stride is shorter than one row"));
        }
        let size = u64::from(stride)
            .checked_mul(u64::from(height))
            .ok_or_else(|| invalid("image_bitmap", "buffer size overflow"))?;
        if size > limits.max_bitmap_bytes {
            return Err(Error::ResourceLimit {
                limit: "max_bitmap_bytes",
                actual: size,
                maximum: limits.max_bitmap_bytes,
            });
        }
        let output_bound = planned_bytes / 4;
        if size > output_bound {
            return Err(invalid("image_bitmap", "decoded size exceeded preflight plan"));
        }
        let capacity = usize::try_from(size)
            .map_err(|_| invalid("image_bitmap", "buffer does not fit usize"))?;
        let source = unsafe { (self.bitmap_buffer)(bitmap.raw) }.cast::<u8>();
        if source.is_null() && capacity != 0 {
            return Err(self.error("image_bitmap_buffer"));
        }
        let mut bytes = zeroed_boxed_bytes(capacity, "image_bitmap")?;
        if capacity != 0 {
            // SAFETY: PDFium reports `stride * height` bytes owned by `bitmap`; the bitmap remains
            // alive through this copy, and all arithmetic/allocation was checked above.
            bytes.copy_from_slice(unsafe { std::slice::from_raw_parts(source, capacity) });
        }
        Ok(ImageBitmap { width, height, stride, format, bytes: bytes.into_vec() })
    }
}
