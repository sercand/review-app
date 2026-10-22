//! Turns the classifier's three logits into a [`ModelOutcome`].
//!
//! The constants were fitted on the reviewer's evaluation corpus, not by
//! shiso, so they live here. The energy gate comes first: an input the model
//! finds out-of-distribution is never trusted, whatever its argmax says.

use review_approval::model_stage::ModelOutcome;

/// Softmax temperature (fitted digits kept verbatim).
#[allow(clippy::excessive_precision)]
const TEMPERATURE: f32 = 1.336_620_330_810_546_9;
/// Energy above this is out-of-distribution (95th percentile of in-distribution).
#[allow(clippy::excessive_precision)]
const ENERGY_P95: f32 = -2.543_365_240_097_046;
/// `P(safe)` must exceed this for `Safe`.
const SAFE_PROBABILITY: f32 = 0.90;

fn scaled(logits: [f32; 3]) -> [f32; 3] {
    logits.map(|l| l / TEMPERATURE)
}

fn max_of(v: [f32; 3]) -> f32 {
    v.iter().copied().fold(f32::NEG_INFINITY, f32::max)
}

/// Free energy of the temperature-scaled logits.
pub fn energy(logits: [f32; 3]) -> f32 {
    let s = scaled(logits);
    let max = max_of(s);
    let sum_exp: f32 = s.iter().map(|x| (x - max).exp()).sum();
    -(TEMPERATURE * (max + sum_exp.ln()))
}

/// Softmax of the temperature-scaled logits.
pub fn probabilities(logits: [f32; 3]) -> [f32; 3] {
    let s = scaled(logits);
    let max = max_of(s);
    let e = s.map(|x| (x - max).exp());
    let sum: f32 = e.iter().sum();
    e.map(|x| x / sum)
}

pub fn calibrate(logits: [f32; 3]) -> ModelOutcome {
    if logits.iter().any(|l| !l.is_finite()) {
        return ModelOutcome::Unavailable;
    }
    if energy(logits) > ENERGY_P95 {
        return ModelOutcome::NeedsApproval;
    }
    let p = probabilities(logits);
    // First maximum wins ties.
    let mut arg = 0;
    for i in 1..3 {
        if p[i] > p[arg] {
            arg = i;
        }
    }
    match arg {
        0 if p[0] > SAFE_PROBABILITY => ModelOutcome::Safe,
        2 => ModelOutcome::Dangerous,
        _ => ModelOutcome::NeedsApproval,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::fixture;

    #[derive(serde::Deserialize)]
    pub(in crate::ai::local_classifier) struct Golden {
        pub text: String,
        pub logits: [f32; 3],
        pub decision: String,
    }

    pub(in crate::ai::local_classifier) fn goldens() -> Vec<Golden> {
        serde_json::from_str(fixture!("classifier_goldens.json")).unwrap()
    }

    pub(in crate::ai::local_classifier) fn expected(decision: &str) -> ModelOutcome {
        match decision {
            "approve" => ModelOutcome::Safe,
            "deny" => ModelOutcome::Dangerous,
            "defer" => ModelOutcome::NeedsApproval,
            other => panic!("unknown decision {other}"),
        }
    }

    #[test]
    fn matches_every_golden_decision() {
        let rows = goldens();
        assert!(rows.len() >= 150);
        let bad: Vec<_> = rows
            .iter()
            .filter(|g| calibrate(g.logits) != expected(&g.decision))
            .map(|g| g.text.as_str())
            .collect();
        assert!(bad.is_empty(), "mismatches: {bad:?}");
    }

    #[test]
    fn high_confidence_safe_is_safe() {
        assert_eq!(calibrate([6.0, -2.0, -5.0]), ModelOutcome::Safe);
    }

    #[test]
    fn class_two_is_dangerous() {
        assert_eq!(calibrate([-5.0, -2.0, 5.0]), ModelOutcome::Dangerous);
    }

    #[test]
    fn low_confidence_defers() {
        assert_eq!(calibrate([1.0, 0.9, 0.0]), ModelOutcome::NeedsApproval);
    }

    #[test]
    fn ood_energy_defers() {
        assert!(energy([0.0; 3]) > ENERGY_P95);
        assert_eq!(calibrate([0.0; 3]), ModelOutcome::NeedsApproval);
    }

    #[test]
    fn nan_is_unavailable() {
        assert_eq!(calibrate([f32::NAN, 0.0, 0.0]), ModelOutcome::Unavailable);
        assert_eq!(
            calibrate([f32::INFINITY, 0.0, 0.0]),
            ModelOutcome::Unavailable
        );
    }
}
