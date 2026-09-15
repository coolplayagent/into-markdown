use super::*;

#[repr(C)]
#[derive(Default)]
pub(super) struct Matrix {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Native {
    pub(super) fn character_font_size(&self, text: usize, index: c_int) -> Result<f64, Error> {
        let nominal = unsafe { (self.text_font_size)(text as Handle, index) };
        let mut matrix = Matrix::default();
        if unsafe { (self.text_char_matrix)(text as Handle, index, &raw mut matrix) } == 0 {
            return Err(self.error("text_char_matrix"));
        }
        scaled_font_size(nominal, matrix.c, matrix.d)
    }

    pub(super) fn character_origin(&self, text: usize, index: c_int) -> Option<(f32, f32)> {
        let (mut x, mut y) = (0.0_f64, 0.0_f64);
        let found =
            unsafe { (self.text_char_origin)(text as Handle, index, &raw mut x, &raw mut y) };
        if found == 0 || [x, y].iter().any(|v| !v.is_finite() || v.abs() > f64::from(f32::MAX)) {
            return None;
        }
        Some((f64_to_f32(x), f64_to_f32(y)))
    }

    pub(super) fn character_value(&self, text: usize, index: c_int) -> char {
        let raw = if unsafe { (self.text_is_hyphen)(text as Handle, index) } == 1 {
            u32::from(b'-')
        } else {
            unsafe { (self.text_unicode)(text as Handle, index) }
        };
        char::from_u32(raw)
            .filter(|value| {
                *value != '\0' && (!value.is_control() || matches!(value, '\n' | '\r' | '\t'))
            })
            .unwrap_or('\u{fffd}')
    }
}

fn scaled_font_size(nominal: f64, c: f32, d: f32) -> Result<f64, Error> {
    let scale = f64::from(c).hypot(f64::from(d));
    let size = nominal * scale;
    if !size.is_finite() || nominal < 0.0 || size > f64::from(f32::MAX) {
        return Err(invalid("character_style", "transformed font size is not representable"));
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_uses_the_effective_vertical_transform() {
        assert_eq!(scaled_font_size(1.0, 0.0, 9.5).unwrap(), 9.5);
        assert_eq!(scaled_font_size(9.5, 0.0, 1.0).unwrap(), 9.5);
        assert_eq!(scaled_font_size(5.0, -2.0, 0.0).unwrap(), 10.0);
        assert!(scaled_font_size(f64::NAN, 0.0, 1.0).is_err());
        assert!(scaled_font_size(-1.0, 0.0, 1.0).is_err());
        assert!(scaled_font_size(f64::MAX, 0.0, 2.0).is_err());
    }
}
