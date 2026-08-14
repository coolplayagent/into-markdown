use super::{PageInfo, Rect};

const COVERAGE_GRID: usize = 64;

#[derive(Clone)]
pub(super) struct PageCoverage {
    occupied: [bool; COVERAGE_GRID * COVERAGE_GRID],
}

impl Default for PageCoverage {
    fn default() -> Self {
        Self { occupied: [false; COVERAGE_GRID * COVERAGE_GRID] }
    }
}

impl PageCoverage {
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn add(&mut self, bounds: Rect, info: &PageInfo) {
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return;
        }
        let cell_width = info.width_points / COVERAGE_GRID as f32;
        let cell_height = info.height_points / COVERAGE_GRID as f32;
        for row in 0..COVERAGE_GRID {
            let top = row as f32 * cell_height;
            let bottom = top + cell_height;
            for column in 0..COVERAGE_GRID {
                let left = column as f32 * cell_width;
                let right = left + cell_width;
                if left >= bounds.x
                    && right <= bounds.x + bounds.width
                    && top >= bounds.y
                    && bottom <= bounds.y + bounds.height
                {
                    self.occupied[row * COVERAGE_GRID + column] = true;
                }
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    pub(super) fn ratio(&self) -> f64 {
        self.occupied.iter().filter(|occupied| **occupied).count() as f64
            / self.occupied.len() as f64
    }
}
