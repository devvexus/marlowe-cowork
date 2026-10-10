//! The GPU rerank cascade: depth 30, narrowed to 10 by the shipped graph, then reciprocal-rank
//! fusion over six fine-tuned cross-encoders.
//!
//! Old harness, M0c Session M2, held-out n = 229 (`tools/cascade_squeeze.py`, pre-registered):
//! **R@3 0.8908** against the shipped single-graph path's **0.8515**; McNemar 11 gained / 2 lost,
//! p = 0.0225. Input recall 0.9039 -> 0.9782 from the deeper slate.
//!
//! **The membership is a rule, not a selection**: every graph trained on Session J's recipe (same
//! pairs, MarginMSE, delta rule, seed and fold), with no subset chosen. On fit it ranked second
//! behind L-4-ft-w1 alone; held-out L-4 alone came last. Do not "improve" the list by scoring
//! candidates on the fit split. The `ft-m-*` family is excluded: different negative mining.
//!
//! **GPU only, by measurement**: on the shipped CPU pins (1 thread, batch 1) the cascade read
//! p50 1134 ms against the 300 ms budget. The product takes it only when the reranker loads on a
//! GPU (see `marlowe-mind`); the CPU path stays the shipped depth-10 single graph.
//!
//! `rerank_score` keeps the SHIPPED graph's logit under the cascade: the operating point is
//! calibrated in those units. The fusion's opinion is `fusion_rank`, a separate field.

use std::path::Path;

use crate::ep::Provider;
use crate::rerank::{CrossEncoder, RerankError};

/// The six fused graphs and their `model.onnx` digests. The first is the shipped graph and the
/// narrower (its digest is [`crate::rerank::MODEL_SHA256`]); the 30 -> 10 narrowing was measured
/// with it (retention 0.9956). Order matters: RRF sums in this order, as the offline tool did.
pub const FUSION_GRAPHS: &[(&str, &str)] = &[
    ("ms-marco-MiniLM-L-2-v2-ft-session-j", "9c222dac4315cfd2f33f2e865bb651a7a16bf532c11e55b7fb1a43bb041880c0"),
    ("ms-marco-MiniLM-L-6-v2-ft-session-j", "78dcc7c1834b2e0cfc67d58a2735b9bc27900f46d5b5ccada99e8ee610c724f6"),
    ("ms-marco-MiniLM-L-2-v2-ft-w1", "cc4df68c30d319cab3d74c166f4de18c4ade7e8c1fccd1fbc1512bdffee58b96"),
    ("ms-marco-MiniLM-L-4-v2-ft-w1", "f6324276380fbc6a6a34864c077a94d5227d9719d9d5dfbc80bcb8695078fddb"),
    ("ms-marco-MiniLM-L-6-v2-ft-w1", "bbfb8a0831db882397c2b83f1c108e233320e1c257c923216357d2c48e25e050"),
    ("ms-marco-MiniLM-L-12-v2-ft-w1", "b1a374f3cd2752af2143d4f01853282a9a1d4e7ee8137e82c5827291b26ccdd6"),
];

/// Reciprocal-rank-fusion constant: Cormack et al.'s default, fixed before any number existed.
pub const RRF_K: f64 = 60.0;

/// The slate the cascade draws.
pub const CASCADE_SLATE: usize = 30;

// The slate runs as whole measured batches.
const _: () = assert!(CASCADE_SLATE.is_multiple_of(crate::rerank::MAX_BATCH));

/// How many candidates the shipped graph keeps for the fusion. Equal to the shipped rerank
/// budget, so the final ranker sees as many candidates as the CPU path does.
pub const CASCADE_NARROW: usize = 10;

/// The five graphs scored after the shipped one narrows, all on one provider.
pub struct Cascade {
    members: Vec<CrossEncoder>,
}

impl Cascade {
    /// Load every non-shipped member from `<models_dir>\<name>`, digest-pinned, on `provider`.
    pub fn load(models_dir: &Path, threads: usize, provider: Provider) -> Result<Self, RerankError> {
        let members = FUSION_GRAPHS[1..]
            .iter()
            .map(|(name, _)| CrossEncoder::load_fusion_member(&models_dir.join(name), name, threads, provider))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { members })
    }

    pub fn members_mut(&mut self) -> &mut [CrossEncoder] {
        &mut self.members
    }

    pub fn provider(&self) -> Provider {
        self.members.first().map_or(Provider::Cpu, CrossEncoder::provider)
    }

    /// The member directories a models dir must hold.
    pub fn required_dirs() -> impl Iterator<Item = &'static str> {
        FUSION_GRAPHS[1..].iter().map(|(n, _)| *n)
    }
}

/// Rank of each position of `narrow` under `logits` (descending), ties by candidate index.
fn ranks(narrow: &[usize], logits: &[f32]) -> Vec<usize> {
    let mut by: Vec<usize> = (0..narrow.len()).collect();
    by.sort_by(|&a, &b| logits[b].total_cmp(&logits[a]).then_with(|| narrow[a].cmp(&narrow[b])));
    let mut rank = vec![0usize; narrow.len()];
    for (r, &p) in by.iter().enumerate() {
        rank[p] = r;
    }
    rank
}

/// The fusion, a pure function of ranks, replicating `tools/cascade_squeeze.py`'s `rrf`:
///
/// * `narrow` arrives in shipped-logit order, so position `p` holds the shipped graph's rank `p`;
/// * each member ranks the narrowed set by its own logits (desc), ties by candidate index;
/// * score = sum over graphs, in [`FUSION_GRAPHS`] order, of `1 / (RRF_K + rank)`, in f64 as
///   Python summed it;
/// * output `(candidate index, fusion rank)`, fusion score desc, ties by candidate index.
///
/// `members[m]` holds member `m`'s logits by position in `narrow`.
pub fn fuse(narrow: &[usize], members: &[Vec<f32>]) -> Vec<(usize, u32)> {
    let member_ranks: Vec<Vec<usize>> = members.iter().map(|l| ranks(narrow, l)).collect();
    let score = |p: usize| -> f64 {
        let mut s = 1.0 / (RRF_K + p as f64);
        for r in &member_ranks {
            s += 1.0 / (RRF_K + r[p] as f64);
        }
        s
    };
    let scores: Vec<f64> = (0..narrow.len()).map(score).collect();
    let mut order: Vec<usize> = (0..narrow.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then_with(|| narrow[a].cmp(&narrow[b])));
    order.into_iter().enumerate().map(|(rank, p)| (narrow[p], rank as u32)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_graph_heads_the_fusion_and_its_digest_is_the_pinned_one() {
        assert_eq!(FUSION_GRAPHS.len(), 6);
        assert_eq!(FUSION_GRAPHS[0].1, crate::rerank::MODEL_SHA256);
        assert_eq!(Cascade::required_dirs().count(), 5);
        assert_eq!(CASCADE_NARROW, crate::retrieve::RERANK_BUDGET);
    }

    #[test]
    fn with_no_members_the_fusion_is_the_shipped_order() {
        let narrow = [7usize, 3, 9];
        assert_eq!(fuse(&narrow, &[]), vec![(7, 0), (3, 1), (9, 2)]);
    }

    #[test]
    fn rrf_arithmetic_matches_hand_computed_values() {
        // Shipped ranks: c10=0, c20=1, c30=2. One member reverses them: c30=0, c20=1, c10=2.
        // c10 and c30 both score 1/60 + 1/62; c20 scores 2/61, which is less than 1/60 + 1/62.
        // The tie between c10 and c30 goes to the LOWER candidate index.
        let narrow = [10usize, 20, 30];
        let member = vec![vec![-1.0f32, 0.0, 1.0]];
        assert_eq!(fuse(&narrow, &member), vec![(10, 0), (30, 1), (20, 2)]);
    }

    #[test]
    fn a_unanimous_member_reinforces_and_ties_in_a_member_break_by_index() {
        let narrow = [5usize, 2, 8];
        // Member ties all three logits: its ranks fall back to candidate index (2, 5, 8).
        let member = vec![vec![0.5f32, 0.5, 0.5]];
        // Scores: c5 = 1/60 + 1/61, c2 = 1/61 + 1/60, c8 = 2/62. c5 and c2 tie; lower index first.
        assert_eq!(fuse(&narrow, &member), vec![(2, 0), (5, 1), (8, 2)]);
    }
}
