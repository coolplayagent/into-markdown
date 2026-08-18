use super::{SemanticMetrics, SemanticNode};
use std::collections::BTreeSet;

pub(super) fn metrics(golden: &[SemanticNode], actual: &[SemanticNode]) -> SemanticMetrics {
    let golden = identities(golden);
    let actual = identities(actual);
    let true_positive = golden.intersection(&actual).count() as u64;
    let false_positive = actual.difference(&golden).count() as u64;
    let false_negative = golden.difference(&actual).count() as u64;
    SemanticMetrics {
        precision: ratio(true_positive, true_positive + false_positive),
        recall: ratio(true_positive, true_positive + false_negative),
        true_positive,
        false_positive,
        false_negative,
    }
}

fn identities(nodes: &[SemanticNode]) -> BTreeSet<(&str, &str, &str)> {
    nodes.iter().map(|node| (node.id.as_str(), node.kind, node.text.as_str())).collect()
}

#[allow(
    clippy::cast_precision_loss,
    reason = "validated Document IR contains at most 100,000 semantic nodes"
)]
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 { 1.0 } else { numerator as f64 / denominator as f64 }
}
