use super::*;

pub(super) fn web_options() -> ConversionOptions {
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = crate::config::memory::probe().auto_budget_bytes;
    crate::config::memory::apply_asset_defaults(false, false, &mut options);
    options
}

pub(super) fn deserialize_web_options<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ConversionOptions, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut options: ConversionOptions =
        serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
    let limits = value.get("limits").and_then(serde_json::Value::as_object);
    let explicit = |key: &str| limits.is_some_and(|fields| fields.contains_key(key));
    if !explicit("max_memory_bytes") {
        options.limits.max_memory_bytes = crate::config::memory::probe().auto_budget_bytes;
    }
    crate::config::memory::apply_asset_defaults(
        explicit("max_asset_bytes"),
        explicit("max_total_asset_bytes"),
        &mut options,
    );
    Ok(options)
}
