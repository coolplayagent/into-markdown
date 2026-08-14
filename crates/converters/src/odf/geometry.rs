use crate::odf::model::{SVG_NS, malformed};
use crate::odf::xml::XmlNode;
use into_markdown_core::{ConversionError, Rect};

#[derive(Clone, Copy, Debug)]
pub(super) struct Transform {
    pub(super) a: f32,
    pub(super) b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Transform {
    pub(super) const IDENTITY: Self = Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    pub(super) fn then(self, local: Self) -> Result<Self, ConversionError> {
        let result = Self {
            a: self.a * local.a + self.c * local.b,
            b: self.b * local.a + self.d * local.b,
            c: self.a * local.c + self.c * local.d,
            d: self.b * local.c + self.d * local.d,
            e: self.a * local.e + self.c * local.f + self.e,
            f: self.b * local.e + self.d * local.f + self.f,
        };
        result.validate("composed draw:transform")?;
        Ok(result)
    }

    fn point(self, x: f32, y: f32) -> Result<(f32, f32), ConversionError> {
        let point = (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f);
        if point.0.is_finite() && point.1.is_finite() {
            Ok(point)
        } else {
            Err(malformed(Some("content.xml"), "non-finite transformed drawing point"))
        }
    }

    fn validate(self, field: &str) -> Result<(), ConversionError> {
        if [self.a, self.b, self.c, self.d, self.e, self.f].iter().all(|value| value.is_finite()) {
            Ok(())
        } else {
            Err(malformed(Some("content.xml"), format!("non-finite {field}")))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn transform_bounds(
    bounds: Rect,
    transform: Transform,
) -> Result<Rect, ConversionError> {
    let points = [
        transform.point(bounds.x, bounds.y)?,
        transform.point(bounds.x + bounds.width, bounds.y)?,
        transform.point(bounds.x, bounds.y + bounds.height)?,
        transform.point(bounds.x + bounds.width, bounds.y + bounds.height)?,
    ];
    let min_x = points.iter().map(|point| point.0).fold(f32::INFINITY, f32::min);
    let max_x = points.iter().map(|point| point.0).fold(f32::NEG_INFINITY, f32::max);
    let min_y = points.iter().map(|point| point.1).fold(f32::INFINITY, f32::min);
    let max_y = points.iter().map(|point| point.1).fold(f32::NEG_INFINITY, f32::max);
    let result = Rect { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y };
    if [result.x, result.y, result.width, result.height].iter().all(|value| value.is_finite())
        && result.width >= 0.0
        && result.height >= 0.0
    {
        Ok(result)
    } else {
        Err(malformed(Some("content.xml"), "non-finite transformed drawing bounds"))
    }
}

pub(super) fn drawing_bounds(node: &XmlNode) -> Result<Option<Rect>, ConversionError> {
    let values = [
        node.attr(SVG_NS, "x"),
        node.attr(SVG_NS, "y"),
        node.attr(SVG_NS, "width"),
        node.attr(SVG_NS, "height"),
    ];
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    let [Some(x), Some(y), Some(width), Some(height)] = values else {
        return Err(malformed(Some("content.xml"), "drawing bounds are incomplete"));
    };
    let result = Rect {
        x: parse_length(x)?,
        y: parse_length(y)?,
        width: parse_length(width)?,
        height: parse_length(height)?,
    };
    if result.width < 0.0 || result.height < 0.0 {
        return Err(malformed(Some("content.xml"), "drawing dimensions are negative"));
    }
    Ok(Some(result))
}

fn parse_length(value: &str) -> Result<f32, ConversionError> {
    let split = value
        .find(|character: char| {
            !(character.is_ascii_digit() || matches!(character, '.' | '-' | '+'))
        })
        .unwrap_or(value.len());
    let number = value[..split]
        .parse::<f32>()
        .map_err(|_| malformed(Some("content.xml"), "invalid drawing length"))?;
    if !number.is_finite() {
        return Err(malformed(Some("content.xml"), "non-finite drawing length"));
    }
    let points = match &value[split..] {
        "cm" => number * 72.0 / 2.54,
        "mm" => number * 72.0 / 25.4,
        "in" => number * 72.0,
        "pt" => number,
        "pc" => number * 12.0,
        "px" => number * 0.75,
        _ => return Err(malformed(Some("content.xml"), "unsupported drawing length unit")),
    };
    if points.is_finite() {
        Ok(points)
    } else {
        Err(malformed(Some("content.xml"), "drawing length overflows finite bounds"))
    }
}

pub(super) fn parse_transform(value: Option<&str>) -> Result<Transform, ConversionError> {
    let Some(mut rest) = value else { return Ok(Transform::IDENTITY) };
    let mut result = Transform::IDENTITY;
    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        let open = rest
            .find('(')
            .ok_or_else(|| malformed(Some("content.xml"), "invalid draw:transform"))?;
        let close = rest[open + 1..]
            .find(')')
            .map(|offset| open + 1 + offset)
            .ok_or_else(|| malformed(Some("content.xml"), "unterminated draw:transform"))?;
        let name = &rest[..open];
        let args: Vec<_> = rest[open + 1..close]
            .split(|character: char| character == ',' || character.is_whitespace())
            .filter(|value| !value.is_empty())
            .collect();
        let local = match (name, args.as_slice()) {
            ("rotate", [angle]) => {
                let angle = parse_finite(angle, "rotation")?;
                let (sin, cos) = angle.sin_cos();
                Transform { a: cos, b: sin, c: -sin, d: cos, e: 0.0, f: 0.0 }
            }
            ("translate", [x]) => Transform { e: parse_length(x)?, ..Transform::IDENTITY },
            ("translate", [x, y]) => {
                Transform { e: parse_length(x)?, f: parse_length(y)?, ..Transform::IDENTITY }
            }
            ("scale", [x]) => {
                let value = parse_finite(x, "scale")?;
                Transform { a: value, d: value, ..Transform::IDENTITY }
            }
            ("scale", [x, y]) => Transform {
                a: parse_finite(x, "scale")?,
                d: parse_finite(y, "scale")?,
                ..Transform::IDENTITY
            },
            ("matrix", [scale_x, shear_y, shear_x, scale_y, offset_x, offset_y]) => Transform {
                a: parse_finite(scale_x, "matrix")?,
                b: parse_finite(shear_y, "matrix")?,
                c: parse_finite(shear_x, "matrix")?,
                d: parse_finite(scale_y, "matrix")?,
                e: parse_length_or_number(offset_x)?,
                f: parse_length_or_number(offset_y)?,
            },
            _ => return Err(malformed(Some("content.xml"), "unsupported draw:transform")),
        };
        result = result.then(local)?;
        rest = &rest[close + 1..];
    }
    if [result.a, result.b, result.c, result.d, result.e, result.f]
        .iter()
        .any(|value| !value.is_finite())
    {
        return Err(malformed(Some("content.xml"), "non-finite composed draw:transform"));
    }
    Ok(result)
}

fn parse_finite(value: &str, field: &str) -> Result<f32, ConversionError> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| malformed(Some("content.xml"), format!("invalid drawing {field}")))
}

fn parse_length_or_number(value: &str) -> Result<f32, ConversionError> {
    if value.chars().all(|character| {
        character.is_ascii_digit() || matches!(character, '.' | '-' | '+' | 'e' | 'E')
    }) {
        parse_finite(value, "matrix offset")
    } else {
        parse_length(value)
    }
}
