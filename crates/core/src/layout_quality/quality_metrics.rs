use super::{SemanticMetrics, SemanticNode, working_overflow};
use crate::{ConversionError, ExecutionContext};
use std::collections::BTreeSet;

pub(super) fn metrics(
    golden: &[SemanticNode],
    actual: &[SemanticNode],
    context: &ExecutionContext,
) -> Result<SemanticMetrics, ConversionError> {
    // Identity construction plus the three set comparisons are all bounded.
    let units = golden
        .len()
        .checked_add(actual.len())
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(working_overflow)?;
    context.consume_work(units)?;
    let golden = identities(golden);
    let actual = identities(actual);
    let true_positive = golden.intersection(&actual).count() as u64;
    let false_positive = actual.difference(&golden).count() as u64;
    let false_negative = golden.difference(&actual).count() as u64;
    Ok(SemanticMetrics {
        precision: ratio(true_positive, true_positive + false_positive),
        recall: ratio(true_positive, true_positive + false_negative),
        true_positive,
        false_positive,
        false_negative,
    })
}

fn identities(nodes: &[SemanticNode]) -> BTreeSet<(&str, &str, &str)> {
    nodes.iter().map(|node| (node.id.as_str(), node.kind.as_str(), node.text.as_str())).collect()
}

#[allow(
    clippy::cast_precision_loss,
    reason = "validated Document IR contains at most 100,000 semantic nodes"
)]
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 { 1.0 } else { numerator as f64 / denominator as f64 }
}
